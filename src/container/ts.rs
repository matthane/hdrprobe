//! MPEG-2 Transport Stream (`.ts`) and BDAV M2TS (`.m2ts`/`.mts`) demuxer.
//!
//! Transport streams scatter the elementary stream across fixed-size packets
//! (188 bytes for TS; 192 for M2TS, a 4-byte copy-control prefix + 188), so —
//! unlike every other backend — the video payload is *not* a contiguous byte
//! range in the file. We therefore **reassemble** sampled access units out of
//! the scattered PES payloads into an owned buffer (`Demux::reassembled`), and
//! the chunk offsets index into that buffer.
//!
//! We never decode. We parse PAT → PMT to find the video PID(s) (including a
//! Profile-7 enhancement-layer PID flagged by a Dolby Vision descriptor),
//! reassemble a bounded head sample of access units, and hand the resulting
//! Annex-B buffer to the normal NAL walker for RPU/SEI extraction. Under
//! `--full` the whole video elementary stream is *streamed*, not materialized:
//! demux exposes `TsFullStream` on `Demux::ts_stream` and `sample::scan` drives
//! the resumable `EsStreamer` through the file in `STREAM_WINDOW_BYTES` windows
//! against one reused scratch buffer, so peak heap stays bounded no matter the
//! file size (a UHD BD M2TS video track runs tens of GB).
//!
//! **Duration** has no container box in TS, so — like MediaInfo — we derive it
//! from the transport clock: `last_PCR - first_PCR` on the PCR PID (a 27 MHz
//! reference carried in adaptation fields at least every 100 ms). The first PCR
//! is free from the head window we already read; the last comes from a small
//! bounded tail window (`TAIL_SCAN_BYTES`), which the network prefetch warms in
//! one read alongside the head. The middle of a multi-GB stream is never touched.

use anyhow::{bail, Context, Result};

use crate::avc::nal as avc_nal;
use crate::container::{Chunk, Codec, Demux, DvConfig, NalFormat, TrackDemux};
use crate::hevc::nal::{self, NalRef};
use crate::hevc::sps::{parse_sps, SpsInfo};
use crate::model::{Bitrate, ColorInfo};
use crate::prefetch::Frontier;
use crate::progress::{Phase, Progress};

const SYNC: u8 = 0x47;
const TS_UNIT: usize = 188;
const PID_PAT: u16 = 0x0000;
const STREAM_TYPE_AVC: u8 = 0x1B;
const STREAM_TYPE_HEVC: u8 = 0x24;

/// Bytes from the head the default (`sampled`) demux may read. TS carries no
/// container box, so resolution/colour come only from the in-band SPS, which
/// rides the first IDR — typically ~one 4K GOP (~10 MiB) in, not at byte 0. This
/// window is sized to comfortably cross that first IDR. The network prefetch
/// warms exactly this region for TS files, so a remote scan streams it in one
/// read instead of faulting it in; keep the two in sync (see `prefetch` and the
/// warm-coupling note in CLAUDE.md). `--full` ignores this and scans the whole file.
pub const HEAD_SCAN_BYTES: u64 = 24 << 20; // 24 MiB

/// Bytes from the tail scanned for the last PCR to close out the duration. Sized
/// to comfortably span the ≤100 ms PCR interval at any realistic bitrate (4 MiB
/// covers ~320 Mbps), so at least one PCR on the clock PID lands in it. The
/// prefetch warms exactly this trailing window for TS files (see `prefetch`), so
/// a remote scan streams it in one extra pipelined read rather than faulting the
/// tail in packet-by-packet; keep the two in sync.
pub const TAIL_SCAN_BYTES: u64 = 4 << 20; // 4 MiB

/// A PCR is a 42-bit value: a 33-bit 90 kHz base scaled by 300 plus a 9-bit
/// 27 MHz extension. The 33-bit base wraps roughly every 26.5 h; this is the full
/// modulus in 27 MHz units, used to unwrap a single tail-before-head rollover.
const PCR_MODULUS: u64 = (1u64 << 33) * 300;

/// The PMT's "no PCR" sentinel PID.
const PID_NONE: u16 = 0x1FFF;

/// Packet layout: offset of the first sync byte and the packet stride.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    first: usize,
    stride: usize,
}

/// Detect TS (188) vs M2TS (192) framing by locking onto 5 consecutive sync
/// bytes at a fixed stride. Returns the first sync offset and the stride.
pub fn detect_layout(data: &[u8]) -> Option<Layout> {
    for &stride in &[TS_UNIT, 192usize] {
        if data.len() < 4 * stride + 1 {
            continue;
        }
        for first in 0..stride {
            if (0..5).all(|k| data.get(first + k * stride) == Some(&SYNC)) {
                return Some(Layout { first, stride });
            }
        }
    }
    None
}

pub fn demux(data: &[u8], full: bool, progress: &Progress, frontier: &Frontier) -> Result<Demux> {
    let layout = detect_layout(data).context("not a recognized TS/M2TS stream")?;
    let programs = parse_psi(data, layout).context("no PMT / program map found")?;
    let groups = group_video_pids(&programs);
    if groups.is_empty() {
        bail!("no HEVC/AVC/Dolby Vision video PID in the program map");
    }

    // Metadata always comes from the bounded head pass — even under `--full`,
    // which streams the whole elementary stream through `sample::scan` in
    // bounded windows (`TsFullStream`) instead of materializing it here. One
    // walk reassembles every group's PIDs, routed into per-group buffers. The
    // packet budget is sized to `HEAD_SCAN_BYTES` for one video stream; with N
    // independent streams interleaved that budget covers ~1/N of each stream's
    // ES and a later stream's first IDR/SPS would fall outside, so it scales
    // with the group count (capped). Only multi-video files pay: the prefetch
    // head warm stays exactly `HEAD_SCAN_BYTES`, and the scaled overflow on
    // those rare files is a bounded cold read.
    let group_pids: Vec<Vec<u16>> = groups.iter().map(|g| g.pids()).collect();
    let budget = (HEAD_SCAN_BYTES / 192) as usize * groups.len().min(HEAD_BUDGET_MAX_SCALE);
    let outs = head_reassemble(data, layout, &group_pids, budget);

    // Per-group codec + widest SPS from the group's own buffer.
    let codecs: Vec<Codec> = groups.iter().map(|g| group_codec(&g.streams)).collect();
    let mut bests: Vec<Option<SpsCommon>> =
        outs.iter().zip(&codecs).map(|(o, c)| best_sps(&o.buf, &o.chunks, c)).collect();
    let mut sps_chunks: Vec<Option<usize>> =
        bests.iter().map(|b| b.as_ref().map(|c| c.chunk)).collect();
    if full && bests.iter().any(|b| b.is_none()) {
        // A group's head window held no SPS at all (first IDR beyond the
        // budget — atypical captures): under `--full` keep looking through the
        // whole stream rather than losing the resolution the old whole-stream
        // pass would have found — one walk fills every missing group. A rescue
        // hit's chunk index is window-relative, meaningless against the head
        // chunks — and unneeded: the `--full` scan covers every AU, so nothing
        // must be pinned.
        let targets: Vec<(usize, Vec<u16>, Codec)> = bests
            .iter()
            .enumerate()
            .filter(|(_, b)| b.is_none())
            .map(|(i, _)| (i, group_pids[i].clone(), codecs[i].clone()))
            .collect();
        for (i, rescued) in sps_rescue(data, layout, &targets, progress, frontier) {
            bests[i] = rescued;
            sps_chunks[i] = None;
        }
    }

    // Duration from the transport clock (head+tail PCR delta), file-level: a
    // multi-program capture shares one mux timeline. Prefer the PMTs' declared
    // PCR PIDs, falling back to the video PID(s) — most streams carry the PCR
    // on the video PID anyway.
    let duration_secs = programs
        .iter()
        .map(|p| p.pcr_pid)
        .chain(group_pids.iter().flatten().copied())
        .filter(|&pid| pid != PID_NONE)
        .find_map(|pid| pcr_duration(data, layout, pid));

    let container = if layout.stride == 192 {
        "MPEG-2 TS (M2TS/BDAV)"
    } else {
        "MPEG-2 TS"
    };

    let ts_stream = full.then(|| TsFullStream { layout, groups: group_pids.clone() });
    let multi_program = programs.len() > 1;
    let single = groups.len() == 1;

    let mut tracks = Vec::with_capacity(groups.len());
    for (((g, out), best), codec) in
        groups.iter().zip(outs).zip(bests.into_iter().zip(sps_chunks)).zip(codecs)
    {
        let (best, sps_chunk) = best;
        let (width, height, bit_depth, chroma, codec_profile, color, fps) = sps_fields(best);

        // `--full`: the exact video-stream byte total is only known after the
        // sampler's streaming walk, so leave the rate unset here — main.rs
        // fills it from `sample::Scan` (the same value the old whole-stream
        // reassembly produced, including `None` when no video bytes complete).
        // The default bounded path reports the file-length overall rate as
        // before — but only when this is the file's only video track: an
        // overall rate (audio + overhead included) attributed to one of
        // several tracks would be a wrong number, so multi-track reports
        // `None` instead.
        let bitrate = if full || !single {
            None
        } else {
            Bitrate::overall(data.len() as u64, duration_secs)
        };

        tracks.push(TrackDemux {
            track_number: Some(g.primary_pid() as u64),
            program: multi_program.then_some(g.program_number),
            width,
            height,
            fps,
            bit_depth,
            chroma,
            codec_profile,
            color,
            dv_config: g.streams.iter().find_map(|e| e.dv_config.clone()),
            dv_dual_track: g.dv_dual_track,
            bitrate,
            chunks: out.chunks,
            sps_chunk,
            reassembled: Some(out.buf),
            ..TrackDemux::new(codec, NalFormat::AnnexB)
        });
    }

    Ok(Demux {
        container,
        duration_secs,
        tracks,
        ts_stream,
        mkv_stream: None,
        raw_stream: None,
    })
}

/// Cap on the head packet-budget scaling for multi-video files. 3× covers the
/// realistic worst case (a whole-mux capture's per-service video share) without
/// letting a pathological PMT force an unbounded head read.
const HEAD_BUDGET_MAX_SCALE: usize = 3;

/// Codec of a group's base layer: the PMT stream_type is authoritative (0x1B
/// AVC, 0x24 HEVC). A DV-only PID (EL, PES-private 0x06) carries no video
/// type, so fall back to the DV profile — only profile 9 is AVC. HEVC wins a
/// tie (an AVC EL alongside an HEVC BL is not a real configuration, but be
/// explicit).
fn group_codec(streams: &[Es]) -> Codec {
    let has = |t: u8| streams.iter().any(|e| e.stream_type == t);
    if has(STREAM_TYPE_HEVC) {
        Codec::Hevc
    } else if has(STREAM_TYPE_AVC)
        || streams.iter().find_map(|e| e.dv_config.as_ref()).map(|c| c.profile) == Some(9)
    {
        Codec::Avc
    } else {
        Codec::Hevc
    }
}

// --- PSI (PAT / PMT) --------------------------------------------------------

/// One elementary stream from the PMT.
#[derive(Debug, Clone)]
struct Es {
    pid: u16,
    stream_type: u8,
    has_dovi: bool,
    dv_config: Option<DvConfig>,
    /// The 0xB0 descriptor's `dependency_pid` — the base-layer PID this
    /// EL/RPU-only stream enhances. Present only on the EL form
    /// (`bl_present == 0`); names the group the EL folds into.
    dependency_pid: Option<u16>,
}

/// One program from the PAT/PMT walk.
struct Program {
    program_number: u16,
    pcr_pid: u16,
    streams: Vec<Es>,
}

/// One reported (logical) video track: the PIDs whose PES payloads reassemble
/// into its elementary stream — the base layer's, plus a folded DV
/// enhancement layer's in the dual-PID Profile 7 case.
struct PidGroup {
    program_number: u16,
    /// Video-ish streams feeding this track, base layer first.
    streams: Vec<Es>,
    dv_dual_track: bool,
}

impl PidGroup {
    fn pids(&self) -> Vec<u16> {
        self.streams.iter().map(|e| e.pid).collect()
    }

    /// The track's identity PID: the base layer's (the first video-typed
    /// stream, else the first stream).
    fn primary_pid(&self) -> u16 {
        self.streams
            .iter()
            .find(|e| is_video_type(e.stream_type))
            .unwrap_or(&self.streams[0])
            .pid
    }
}

fn is_video_type(t: u8) -> bool {
    t == STREAM_TYPE_HEVC || t == STREAM_TYPE_AVC
}

/// A Dolby Vision enhancement-layer stream: its 0xB0 descriptor says the PID
/// carries no base layer, or it is DV-flagged with no video stream type at all
/// (the bare EL/RPU PID shape, PES-private 0x06 with only a DOVI registration
/// descriptor).
fn is_el_stream(e: &Es) -> bool {
    e.dv_config.as_ref().is_some_and(|c| !c.bl_present)
        || (e.has_dovi && !is_video_type(e.stream_type))
}

/// Group each program's video PIDs into reported tracks.
///
/// Within a program: an EL stream folds into its base layer's group — by the
/// descriptor's `dependency_pid` when it names one of the program's video
/// PIDs, else the program's first video-typed PID — setting `dv_dual_track`;
/// every other video PID is its own independent track. A program whose video
/// PIDs carry **no DV descriptor at all** keeps the historical rule: more
/// than one video PID means a descriptor-less BDMV Profile-7 BL+EL pair (an
/// untouched Blu-ray M2TS signals DV via the playlist, not the PMT), so they
/// form one dual-track group rather than independent tracks. Groups come back
/// in program order, then PID order.
fn group_video_pids(programs: &[Program]) -> Vec<PidGroup> {
    let mut groups: Vec<PidGroup> = Vec::new();
    for prog in programs {
        let vids: Vec<&Es> = prog
            .streams
            .iter()
            .filter(|e| is_video_type(e.stream_type) || e.has_dovi)
            .collect();
        if vids.is_empty() {
            continue;
        }
        let any_dv_desc = vids.iter().any(|e| e.has_dovi);
        if !any_dv_desc && vids.len() > 1 {
            // Descriptor-less multi-PID program: the BDMV P7 shape.
            groups.push(PidGroup {
                program_number: prog.program_number,
                streams: vids.into_iter().cloned().collect(),
                dv_dual_track: true,
            });
            continue;
        }
        let (els, base): (Vec<&Es>, Vec<&Es>) = vids.into_iter().partition(|e| is_el_stream(e));
        if base.is_empty() {
            // Only EL/RPU-shaped PIDs (a bare DV PID cut): report what's there.
            groups.push(PidGroup {
                program_number: prog.program_number,
                streams: els.into_iter().cloned().collect(),
                dv_dual_track: false,
            });
            continue;
        }
        let first = groups.len();
        for b in &base {
            groups.push(PidGroup {
                program_number: prog.program_number,
                streams: vec![(*b).clone()],
                dv_dual_track: false,
            });
        }
        for el in els {
            // Fold by dependency_pid when it names a base PID in this program,
            // else into the program's first base group.
            let target = el
                .dependency_pid
                .and_then(|dep| base.iter().position(|b| b.pid == dep))
                .unwrap_or(0);
            let g = &mut groups[first + target];
            g.streams.push(el.clone());
            g.dv_dual_track = true;
        }
    }
    groups
}

/// Payload of a single TS packet at unit offset `p` (`data[p] == 0x47`):
/// its PID, payload-unit-start flag, and payload slice (after any adaptation
/// field). Returns `None` when the packet carries no payload.
fn packet_payload(data: &[u8], off: usize) -> Option<(u16, bool, &[u8])> {
    let b1 = data[off + 1];
    let pusi = b1 & 0x40 != 0;
    let pid = (((b1 & 0x1F) as u16) << 8) | data[off + 2] as u16;
    let afc = (data[off + 3] >> 4) & 0x03;
    let mut start = off + 4;
    if afc & 0x02 != 0 {
        let af_len = data[off + 4] as usize;
        start = off + 5 + af_len;
    }
    let end = off + TS_UNIT;
    if afc & 0x01 == 0 || start >= end || end > data.len() {
        return None;
    }
    Some((pid, pusi, &data[start..end]))
}

/// Walk the head of the stream to resolve every program's PMT PID (from the
/// PAT) and then each program's PCR PID and elementary streams (from its PMT).
/// Programs come back in PAT order; one whose PMT never shows in the scanned
/// head is dropped. A single-program stream — every BDMV, most remuxes — is
/// one entry; a whole-mux broadcast capture yields one per service.
fn parse_psi(data: &[u8], layout: Layout) -> Option<Vec<Program>> {
    let mut progs: Option<Vec<(u16, u16)>> = None; // (program_number, map_pid)
    let mut p = layout.first;
    let mut scanned = 0;
    while p + TS_UNIT <= data.len() && scanned < 20_000 {
        if data[p] == SYNC {
            if let Some((pid, pusi, payload)) = packet_payload(data, p) {
                if pid == PID_PAT && pusi {
                    if let Some(list) = parse_pat(payload) {
                        progs = Some(list);
                        break;
                    }
                }
            }
        }
        p += layout.stride;
        scanned += 1;
    }
    let progs = progs?;

    let mut programs: Vec<Option<Program>> = (0..progs.len()).map(|_| None).collect();
    let mut missing = progs.len();
    let mut p = layout.first;
    let mut scanned = 0;
    while p + TS_UNIT <= data.len() && scanned < 40_000 && missing > 0 {
        if data[p] == SYNC {
            if let Some((pid, pusi, payload)) = packet_payload(data, p) {
                if pusi {
                    if let Some(i) = progs
                        .iter()
                        .position(|&(_, map)| map == pid)
                        .filter(|&i| programs[i].is_none())
                    {
                        if let Some((pcr_pid, streams)) = parse_pmt(payload) {
                            programs[i] =
                                Some(Program { program_number: progs[i].0, pcr_pid, streams });
                            missing -= 1;
                        }
                    }
                }
            }
        }
        p += layout.stride;
        scanned += 1;
    }
    let programs: Vec<Program> = programs.into_iter().flatten().collect();
    (!programs.is_empty()).then_some(programs)
}

/// PAT: every program's `(program_number, program_map_PID)`, in table order
/// (program 0 is the network PID, not a program).
fn parse_pat(payload: &[u8]) -> Option<Vec<(u16, u16)>> {
    let ptr = *payload.first()? as usize;
    let s = payload.get(1 + ptr..)?;
    if *s.first()? != 0x00 {
        return None; // table_id
    }
    let section_length = (((s.get(1)? & 0x0F) as usize) << 8) | *s.get(2)? as usize;
    let prog_end = (3 + section_length).saturating_sub(4).min(s.len()); // exclude CRC
    let mut out = Vec::new();
    let mut i = 8;
    while i + 4 <= prog_end {
        let prog_num = ((s[i] as u16) << 8) | s[i + 1] as u16;
        let map_pid = (((s[i + 2] & 0x1F) as u16) << 8) | s[i + 3] as u16;
        if prog_num != 0 {
            out.push((prog_num, map_pid));
        }
        i += 4;
    }
    (!out.is_empty()).then_some(out)
}

/// PMT: return the PCR PID and the list of elementary streams with DV descriptor
/// info.
fn parse_pmt(payload: &[u8]) -> Option<(u16, Vec<Es>)> {
    let ptr = *payload.first()? as usize;
    let s = payload.get(1 + ptr..)?;
    if *s.first()? != 0x02 {
        return None; // table_id
    }
    let section_length = (((s.get(1)? & 0x0F) as usize) << 8) | *s.get(2)? as usize;
    let prog_end = (3 + section_length).saturating_sub(4).min(s.len());
    let pcr_pid = (((*s.get(8)? & 0x1F) as u16) << 8) | *s.get(9)? as u16;
    let program_info_length = (((*s.get(10)? & 0x0F) as usize) << 8) | *s.get(11)? as usize;

    let mut streams = Vec::new();
    let mut i = 12 + program_info_length;
    while i + 5 <= prog_end {
        let stream_type = s[i];
        let pid = (((s[i + 1] & 0x1F) as u16) << 8) | s[i + 2] as u16;
        let es_info_len = (((s[i + 3] & 0x0F) as usize) << 8) | s[i + 4] as usize;
        let desc = s.get(i + 5..(i + 5 + es_info_len).min(s.len())).unwrap_or(&[]);
        let (has_dovi, dv_config, dependency_pid) = scan_descriptors(desc);
        streams.push(Es { pid, stream_type, has_dovi, dv_config, dependency_pid });
        i += 5 + es_info_len;
    }
    Some((pcr_pid, streams))
}

/// Scan ES-info descriptors for Dolby Vision signalling: a `DOVI` registration
/// descriptor (tag 0x05) and/or the DV video-stream descriptor (tag 0xB0, whose
/// body is a `dvcC`-shaped config record — the EL form of which names its base
/// layer's PID via `dependency_pid`).
fn scan_descriptors(d: &[u8]) -> (bool, Option<DvConfig>, Option<u16>) {
    let mut has_dovi = false;
    let mut cfg = None;
    let mut dep = None;
    let mut i = 0;
    while i + 2 <= d.len() {
        let tag = d[i];
        let len = d[i + 1] as usize;
        let body = d.get(i + 2..(i + 2 + len).min(d.len())).unwrap_or(&[]);
        match tag {
            0x05 => {
                if body.len() >= 4 && &body[0..4] == b"DOVI" {
                    has_dovi = true;
                }
            }
            0xB0 => {
                has_dovi = true;
                if let Some((c, d)) = super::parse_dovi_ts_descriptor(body) {
                    cfg = Some(c);
                    dep = d.or(dep);
                }
            }
            _ => {}
        }
        i += 2 + len;
    }
    (has_dovi, cfg, dep)
}

// --- PES reassembly ---------------------------------------------------------

struct PidState {
    pid: u16,
    /// Index of the track group (and so the `EsOut`) this PID's completed
    /// access units are emitted into.
    group: usize,
    acc: Vec<u8>,
    started: bool,
}

/// One track group's reassembled elementary stream: an Annex-B buffer plus the
/// access-unit ranges indexing into it.
#[derive(Debug, Default)]
pub struct EsOut {
    pub buf: Vec<u8>,
    pub chunks: Vec<Chunk>,
}

impl EsOut {
    pub fn clear(&mut self) {
        self.buf.clear();
        self.chunks.clear();
    }
}

/// Resumable walker over the target PIDs' scattered PES payloads: reassembles
/// completed access units into a caller-provided Annex-B buffer, window by
/// window. One engine serves both paths — the default's bounded head pass
/// (`head_reassemble`) and `--full`'s exhaustive walk, which streams the whole
/// elementary stream through a fixed-size scratch window (`sample::scan`)
/// instead of materializing multi-GB of ES in one owned buffer.
///
/// Partial per-PID accumulators carry across `next_window` calls, so an AU
/// whose packets straddle a window pause is emitted intact in a later window.
/// The trailing AU still accumulating at end of stream is never flushed (no
/// terminating PES start bounds it) — matching the historical one-shot pass,
/// and likewise excluded from the `--full` bitrate byte count. A PID whose PES
/// never restarts accumulates unbounded in `acc`, as it always has; a cap
/// could silently change output if a late PES start eventually flushed it.
pub struct EsStreamer {
    layout: Layout,
    states: Vec<PidState>,
    cursor: usize,
    packets_left: usize,
    finished: bool,
}

impl EsStreamer {
    /// `groups` lists each track group's PIDs; a PID's completed AUs are
    /// emitted into `outs[group]` in `next_window`.
    fn new(layout: Layout, groups: &[Vec<u16>], packet_budget: usize) -> Self {
        EsStreamer {
            layout,
            states: groups
                .iter()
                .enumerate()
                .flat_map(|(group, pids)| {
                    pids.iter().map(move |&pid| PidState {
                        pid,
                        group,
                        acc: Vec::new(),
                        started: false,
                    })
                })
                .collect(),
            cursor: layout.first,
            packets_left: packet_budget,
            finished: false,
        }
    }

    /// Append completed access units to each group's `EsOut` (chunk offsets
    /// are relative to that group's `buf`) until at least `target_bytes`
    /// accumulate across the window (all groups together) or the walk ends
    /// (end of data, packet budget exhausted, or unrecoverable sync loss). A
    /// window may overshoot `target_bytes` by one AU — the whole accumulator
    /// is appended when a new PES start completes it. Returns `true` while
    /// more input remains.
    pub fn next_window(&mut self, data: &[u8], outs: &mut [EsOut], target_bytes: usize) -> bool {
        if self.finished {
            return false;
        }
        let mut appended = 0usize;
        while self.cursor + TS_UNIT <= data.len() && self.packets_left > 0 {
            if data[self.cursor] != SYNC {
                // Re-locking the phase reads no packet, so it costs no budget.
                match resync(data, self.layout, self.cursor) {
                    Some(np) => {
                        self.cursor = np;
                        continue;
                    }
                    None => break,
                }
            }
            if let Some((pid, pusi, payload)) = packet_payload(data, self.cursor) {
                if let Some(si) = self.states.iter().position(|s| s.pid == pid) {
                    let group = self.states[si].group;
                    appended += process(&mut self.states[si], pusi, payload, &mut outs[group]);
                }
            }
            self.cursor += self.layout.stride;
            self.packets_left -= 1;
            if appended >= target_bytes {
                return true;
            }
        }
        self.finished = true;
        false
    }

    /// Absolute byte offset of the walk within the stream — monotonic, ends
    /// within one packet of EOF. Progress reporting's numerator; never affects
    /// parsing.
    pub fn position(&self) -> usize {
        self.cursor
    }
}

/// One-shot head reassembly for the default (non-`--full`) metadata pass: a
/// single window bounded only by `packet_budget` (sized by the caller to
/// `HEAD_SCAN_BYTES` — the prefetch-warmed region, divided by the larger
/// stride, 192, so the byte span stays within it for both TS and M2TS — and
/// scaled, capped, by the independent-group count). The budget is the sole
/// bound: the default just grabs title-stable static metadata, and the read
/// must not be cut short before each stream's first IDR/SPS, typically ~one
/// 4K GOP in.
fn head_reassemble(data: &[u8], layout: Layout, groups: &[Vec<u16>], packet_budget: usize) -> Vec<EsOut> {
    let mut outs: Vec<EsOut> = groups.iter().map(|_| EsOut::default()).collect();
    let mut st = EsStreamer::new(layout, groups, packet_budget);
    st.next_window(data, &mut outs, usize::MAX);
    outs
}

/// Completed-AU bytes per streamed window of the `--full` elementary-stream
/// walk: `sample::scan` drives an `EsStreamer` through the whole file in
/// windows of this size, reusing one scratch buffer. This — plus the head
/// metadata buffer (bounded by `HEAD_SCAN_BYTES`) and the per-PID partial-AU
/// carryover (single-digit MiB) — bounds the owned heap of a `--full` TS scan,
/// replacing the old whole-stream reassembly (the video track's full size,
/// tens of GB for a UHD BD M2TS).
pub const STREAM_WINDOW_BYTES: usize = 64 << 20; // 64 MiB

/// Everything `sample::scan` needs to drive the exhaustive `--full` windowed
/// walk of the video elementary stream without demux having materialized it.
/// Carried on `Demux::ts_stream`, `Some` only for TS/M2TS under `--full`.
#[derive(Debug, Clone)]
pub struct TsFullStream {
    layout: Layout,
    /// PIDs per track group, in `Demux::tracks` order — a P7 BL+EL pair shares
    /// one group; independent video PIDs (per program) get their own.
    groups: Vec<Vec<u16>>,
}

impl TsFullStream {
    #[cfg(test)]
    pub(crate) fn new(layout: Layout, video_pids: Vec<u16>) -> Self {
        TsFullStream { layout, groups: vec![video_pids] }
    }

    /// How many track groups the walk routes into — the caller sizes its
    /// per-group `EsOut` list to this (parallel to `Demux::tracks`).
    pub fn track_count(&self) -> usize {
        self.groups.len()
    }

    /// A fresh unbounded streamer over the whole stream. It starts at the head,
    /// re-reading the window the metadata pass already parsed (<= 24 MiB, cheap)
    /// so every AU is scanned exactly once — by the streamer.
    pub fn streamer(&self) -> EsStreamer {
        EsStreamer::new(self.layout, &self.groups, usize::MAX)
    }
}

/// Feed one packet's payload into a PID's reassembler, emitting a completed
/// access unit (into its group's `EsOut`) when a new PES starts. Returns the
/// bytes appended, the window loop's pacing measure.
fn process(st: &mut PidState, pusi: bool, payload: &[u8], out: &mut EsOut) -> usize {
    let mut appended = 0;
    if pusi {
        // A new PES begins: finalize the previous access unit.
        if st.started && !st.acc.is_empty() {
            let offset = out.buf.len() as u64;
            out.buf.extend_from_slice(&st.acc);
            out.chunks.push(Chunk { offset, size: st.acc.len() as u64 });
            appended = st.acc.len();
        }
        st.acc.clear();
        match pes_es_offset(payload) {
            Some(off) if off < payload.len() => {
                st.acc.extend_from_slice(&payload[off..]);
                st.started = true;
            }
            _ => st.started = false,
        }
    } else if st.started {
        st.acc.extend_from_slice(payload);
    }
    appended
}

/// Offset of the elementary-stream bytes within a PES packet payload (after the
/// 6-byte PES header + the optional PES header extension). `None` if this isn't
/// a PES start.
fn pes_es_offset(payload: &[u8]) -> Option<usize> {
    if payload.len() < 9 || payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return None;
    }
    // payload[3] = stream_id; [4..6] = PES_packet_length; [6] marker '10';
    // [8] = PES_header_data_length.
    let header_data_len = payload[8] as usize;
    Some(9 + header_data_len)
}

/// Snap a desired byte offset up to the next packet boundary on the stream's
/// fixed phase, re-locking if it lands in a null/corrupt region.
fn align(data: &[u8], layout: Layout, ws: usize) -> Option<usize> {
    let Layout { first, stride } = layout;
    if ws <= first {
        return Some(first);
    }
    let k = (ws - first).div_ceil(stride);
    let cand = first + k * stride;
    if cand + TS_UNIT <= data.len() && data[cand] == SYNC {
        Some(cand)
    } else {
        resync(data, layout, cand)
    }
}

/// Search forward from `from` for an offset showing 3 consecutive sync bytes at
/// the packet stride.
fn resync(data: &[u8], layout: Layout, from: usize) -> Option<usize> {
    let stride = layout.stride;
    let limit = (from + 2 * stride).min(data.len());
    (from..limit).find(|&cand| {
        data.get(cand) == Some(&SYNC)
            && data.get(cand + stride) == Some(&SYNC)
            && data.get(cand + 2 * stride) == Some(&SYNC)
    })
}

// --- Duration from the transport clock (PCR) --------------------------------

/// Program Clock Reference recovered from a packet's adaptation field, in 27 MHz
/// units, plus the packet PID and its discontinuity flag. `None` when the packet
/// carries no adaptation field or no PCR.
fn packet_pcr(data: &[u8], off: usize) -> Option<(u16, bool, u64)> {
    let pid = (((data[off + 1] & 0x1F) as u16) << 8) | data[off + 2] as u16;
    let afc = (data[off + 3] >> 4) & 0x03;
    if afc & 0x02 == 0 {
        return None; // no adaptation field
    }
    let af_len = *data.get(off + 4)? as usize;
    if af_len < 7 {
        return None; // too short to hold the flags byte + 6-byte PCR
    }
    let flags = *data.get(off + 5)?;
    let discontinuity = flags & 0x80 != 0;
    if flags & 0x10 == 0 {
        return None; // PCR_flag clear
    }
    let b = data.get(off + 6..off + 12)?;
    let base = ((b[0] as u64) << 25)
        | ((b[1] as u64) << 17)
        | ((b[2] as u64) << 9)
        | ((b[3] as u64) << 1)
        | ((b[4] as u64) >> 7);
    let ext = (((b[4] & 0x01) as u64) << 8) | b[5] as u64;
    Some((pid, discontinuity, base * 300 + ext))
}

/// Duration in seconds from `last_PCR - first_PCR` on `clock_pid`: the first PCR
/// is read from the head, the last from a bounded tail window. Returns `None` if
/// either PCR is missing, a discontinuity is seen in the sampled tail (the clock
/// reset, so the delta is meaningless), or the span is implausible.
fn pcr_duration(data: &[u8], layout: Layout, clock_pid: u16) -> Option<f64> {
    let head_end = (HEAD_SCAN_BYTES as usize).min(data.len());
    let first = scan_pcr(data, layout, clock_pid, layout.first, head_end, false)?;

    let tail_start = data.len().saturating_sub(TAIL_SCAN_BYTES as usize);
    let last = scan_pcr(data, layout, clock_pid, tail_start, data.len(), true)?;

    let span = if last >= first { last - first } else { last + PCR_MODULUS - first };
    let secs = span as f64 / 27_000_000.0;
    // Reject a zero/absurd span (junk PCRs, an undetected mid-file discontinuity,
    // or a second wrap we can't disambiguate) rather than print a wrong number.
    if secs <= 0.0 || secs > 26.0 * 3600.0 {
        return None;
    }
    Some(secs)
}

/// Scan packets in `[start, end)` for PCRs on `clock_pid`. Returns the first such
/// PCR when `want_last` is false, else the last. Bails to `None` if a
/// discontinuity flag is seen (only meaningful for the tail scan, where it means
/// the clock is no longer comparable to the head's).
fn scan_pcr(
    data: &[u8],
    layout: Layout,
    clock_pid: u16,
    start: usize,
    end: usize,
    want_last: bool,
) -> Option<u64> {
    let end = end.min(data.len());
    let mut p = align(data, layout, start)?;
    let mut found: Option<u64> = None;
    while p + TS_UNIT <= end {
        if data[p] != SYNC {
            match resync(data, layout, p) {
                Some(np) => {
                    p = np;
                    continue;
                }
                None => break,
            }
        }
        if let Some((pid, discontinuity, pcr)) = packet_pcr(data, p) {
            if pid == clock_pid {
                if discontinuity {
                    return None;
                }
                if !want_last {
                    return Some(pcr);
                }
                found = Some(pcr);
            }
        }
        p += layout.stride;
    }
    found
}

// --- SPS metadata (no container box in TS) ----------------------------------

/// Common SPS-derived metadata, codec-independent, so the HEVC and AVC scans
/// converge on one shape.
struct SpsCommon {
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma: String,
    profile: String,
    color: ColorInfo,
    frame_rate: Option<f64>,
    /// Index of the chunk the SPS was found in — a RAP access unit, which is
    /// where the per-GOP prefix SEIs ride (see `Demux::sps_chunk`).
    chunk: usize,
}

/// Recover the widest SPS in the reassembled buffer (the base layer outranks a
/// smaller enhancement layer). TS carries no container box, so both colour and
/// frame rate come only from the in-band SPS VUI — parsed with the codec's own
/// SPS reader.
fn best_sps(buf: &[u8], chunks: &[Chunk], codec: &Codec) -> Option<SpsCommon> {
    match codec {
        Codec::Avc => best_avc_sps(buf, chunks),
        _ => best_hevc_sps(buf, chunks),
    }
}

/// Unpack the winning SPS into the demux metadata fields.
#[allow(clippy::type_complexity)]
fn sps_fields(
    best: Option<SpsCommon>,
) -> (u32, u32, Option<u8>, Option<String>, Option<String>, ColorInfo, Option<f64>) {
    match best {
        Some(c) => (
            c.width,
            c.height,
            Some(c.bit_depth),
            Some(c.chroma),
            Some(c.profile),
            c.color,
            c.frame_rate,
        ),
        None => (0, 0, None, None, None, ColorInfo::default(), None),
    }
}

/// `--full` fallback when the head window held no SPS: stream the whole
/// elementary stream, window by window, through the same widest-SPS search,
/// keeping only the running best (each window's buffer is discarded). Stops at
/// the same UHD-width early exit the per-window search uses, else at end of
/// stream. The result's `chunk` index is window-relative — callers must not
/// use it against the head chunks.
fn sps_rescue(
    data: &[u8],
    layout: Layout,
    targets: &[(usize, Vec<u16>, Codec)],
    progress: &Progress,
    frontier: &Frontier,
) -> Vec<(usize, Option<SpsCommon>)> {
    // One walk serves every SPS-less group: their PIDs stream side by side
    // (each target is one group here, so its BL+EL stay merged as in the head
    // pass) and each keeps its own widest-so-far.
    let groups: Vec<Vec<u16>> = targets.iter().map(|(_, pids, _)| pids.clone()).collect();
    let mut st = EsStreamer::new(layout, &groups, usize::MAX);
    let mut outs: Vec<EsOut> = groups.iter().map(|_| EsOut::default()).collect();
    let mut bests: Vec<Option<SpsCommon>> = groups.iter().map(|_| None).collect();
    progress.begin(Phase::Index, data.len() as u64);
    // One window consumes more *file* bytes than its ES target (packet
    // overhead, other PIDs), so the frontier warms the upcoming window's file
    // span, adapted from the last window's observed density.
    let mut warm_span = STREAM_WINDOW_BYTES as u64 * 2;
    loop {
        let pos0 = st.position() as u64;
        frontier.ensure_to(pos0.saturating_add(warm_span));
        let more = st.next_window(data, &mut outs, STREAM_WINDOW_BYTES);
        let used = st.position() as u64 - pos0;
        if used > 0 {
            warm_span = used + used / 4;
        }
        for ((out, best), (_, _, codec)) in outs.iter_mut().zip(&mut bests).zip(targets) {
            if let Some(w) = best_sps(&out.buf, &out.chunks, codec) {
                if best.as_ref().is_none_or(|b| w.width > b.width) {
                    *best = Some(w);
                }
            }
            out.clear();
        }
        if !more || bests.iter().all(|b| b.as_ref().is_some_and(|b| b.width >= 3840)) {
            return targets.iter().map(|(i, _, _)| *i).zip(bests).collect();
        }
        progress.update(st.position() as u64);
    }
}

fn best_hevc_sps(buf: &[u8], chunks: &[Chunk]) -> Option<SpsCommon> {
    let mut best: Option<(usize, SpsInfo)> = None;
    let mut nals: Vec<NalRef> = Vec::new();
    for (ci, c) in chunks.iter().enumerate() {
        let s = c.offset as usize;
        let e = (c.offset + c.size) as usize;
        if e > buf.len() {
            continue;
        }
        nals.clear();
        nal::split_annexb(&buf[s..e], &mut nals);
        for n in &nals {
            if n.nal_type == nal::NAL_SPS {
                if let Some(sps) = parse_sps(&buf[s + n.start..s + n.end]) {
                    if best.as_ref().is_none_or(|(_, b)| sps.width > b.width) {
                        best = Some((ci, sps));
                    }
                }
            }
        }
        if best.as_ref().is_some_and(|(_, b)| b.width >= 3840) {
            break;
        }
    }
    best.map(|(chunk, sps)| SpsCommon {
        width: sps.width,
        height: sps.height,
        bit_depth: sps.bit_depth,
        chroma: sps.chroma_str().to_string(),
        profile: sps.profile_label(),
        color: sps.color.as_ref().map(crate::container::color_from_vui).unwrap_or_default(),
        frame_rate: sps.frame_rate,
        chunk,
    })
}

fn best_avc_sps(buf: &[u8], chunks: &[Chunk]) -> Option<SpsCommon> {
    let mut best: Option<(usize, crate::avc::sps::SpsInfo)> = None;
    let mut nals: Vec<avc_nal::NalRef> = Vec::new();
    for (ci, c) in chunks.iter().enumerate() {
        let s = c.offset as usize;
        let e = (c.offset + c.size) as usize;
        if e > buf.len() {
            continue;
        }
        nals.clear();
        avc_nal::split_annexb(&buf[s..e], &mut nals);
        for n in &nals {
            if n.nal_type == avc_nal::NAL_SPS {
                if let Some(sps) = crate::avc::sps::parse_sps(&buf[s + n.start..s + n.end]) {
                    if best.as_ref().is_none_or(|(_, b)| sps.width > b.width) {
                        best = Some((ci, sps));
                    }
                }
            }
        }
        if best.as_ref().is_some_and(|(_, b)| b.width >= 3840) {
            break;
        }
    }
    best.map(|(chunk, sps)| SpsCommon {
        width: sps.width,
        height: sps.height,
        bit_depth: sps.bit_depth,
        chroma: sps.chroma_str().to_string(),
        profile: sps.profile_label(),
        color: sps.color.as_ref().map(crate::container::color_from_vui).unwrap_or_default(),
        frame_rate: sps.frame_rate,
        chunk,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_with_syncs(first: usize, stride: usize, count: usize) -> Vec<u8> {
        let mut v = vec![0u8; first + stride * (count + 1)];
        for k in 0..count {
            v[first + k * stride] = SYNC;
        }
        v
    }

    #[test]
    fn detect_ts_188() {
        let v = buf_with_syncs(0, 188, 6);
        let l = detect_layout(&v).expect("layout");
        assert_eq!((l.first, l.stride), (0, 188));
    }

    #[test]
    fn detect_m2ts_192() {
        // M2TS syncs live at offset 4 within each 192-byte packet.
        let v = buf_with_syncs(4, 192, 6);
        let l = detect_layout(&v).expect("layout");
        assert_eq!((l.first, l.stride), (4, 192));
    }

    #[test]
    fn pat_returns_program_map_pid() {
        // pointer_field=0, then a one-program PAT for program 1 → PMT PID 0x0100.
        let s = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, // header
            0x00, 0x01, 0xE1, 0x00, // program 1 → 0x0100
            0x00, 0x00, 0x00, 0x00, // CRC
        ];
        let mut payload = vec![0x00]; // pointer_field
        payload.extend_from_slice(&s);
        assert_eq!(parse_pat(&payload), Some(vec![(1, 0x0100)]));
    }

    #[test]
    fn pmt_parses_hevc_and_dovi_streams() {
        // HEVC BL (0x1011, type 0x24) + a DV EL PID (0x1015, type 0x06) carrying a
        // tag-0xB0 DOVI_video_stream_descriptor for the secondary (EL/RPU) PID:
        // profile 7, level 6, rpu+el present, bl absent. Because bl_present_flag
        // is 0, a 16-bit dependency_pid+reserved block (0x80 0x8F) precedes the
        // compat nibble, which is 6 (top nibble of 0x6F). These are the real bytes
        // from testfiles dv7fel_dt_hevc.m2ts. Reading the pre-skip offset would
        // wrongly yield compat 8.
        let s: [u8; 35] = [
            0x02, 0xB0, 0x20, 0x00, 0x01, 0xC1, 0x00, 0x00, // table header
            0xF0, 0x11, 0xF0, 0x00, // PCR PID 0x1011, program_info_length 0
            0x24, 0xF0, 0x11, 0xF0, 0x00, // ES1: HEVC PID 0x1011
            0x06, 0xF0, 0x15, 0xF0, 0x09, // ES2: private PID 0x1015, es_info 9
            0xB0, 0x07, 0x01, 0x00, 0x0E, 0x36, 0x80, 0x8F, 0x6F, // DV descriptor
            0x00, 0x00, 0x00, 0x00, // CRC
        ];
        let mut payload = vec![0x00];
        payload.extend_from_slice(&s);
        let (pcr_pid, streams) = parse_pmt(&payload).expect("pmt");
        assert_eq!(pcr_pid, 0x1011); // PCR PID from the table header
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].pid, 0x1011);
        assert_eq!(streams[0].stream_type, 0x24);
        assert_eq!(streams[1].pid, 0x1015);
        assert!(streams[1].has_dovi);
        let cfg = streams[1].dv_config.as_ref().expect("dv config");
        assert_eq!(cfg.profile, 7);
        assert_eq!(cfg.level, Some(6));
        assert!(cfg.rpu_present && cfg.el_present && !cfg.bl_present);
        assert_eq!(cfg.bl_compatibility_id, Some(6));
        // The EL descriptor's dependency_pid names the base layer's PID.
        assert_eq!(streams[1].dependency_pid, Some(0x1011));
    }

    #[test]
    fn pes_offset_skips_header() {
        // 00 00 01, stream_id, len(2), marker/flags, header_data_length=0.
        let payload = [0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00, 0xAA];
        assert_eq!(pes_es_offset(&payload), Some(9));
        assert_eq!(pes_es_offset(&[0x00, 0x00, 0x00, 0x01]), None);
    }

    #[test]
    fn packet_pcr_decodes_adaptation_field() {
        let base: u64 = 0x1_2345_6789; // 33-bit base
        let ext: u64 = 100; // 9-bit extension
        let mut pkt = vec![0u8; TS_UNIT];
        pkt[0] = SYNC;
        pkt[1] = 0x01; // PID high bits → PID 0x0100
        pkt[2] = 0x00;
        pkt[3] = 0x30; // adaptation field + payload present
        pkt[4] = 7; // adaptation_field_length: flags + 6-byte PCR
        pkt[5] = 0x10; // PCR_flag set, discontinuity clear
        pkt[6] = ((base >> 25) & 0xFF) as u8;
        pkt[7] = ((base >> 17) & 0xFF) as u8;
        pkt[8] = ((base >> 9) & 0xFF) as u8;
        pkt[9] = ((base >> 1) & 0xFF) as u8;
        pkt[10] = (((base & 1) << 7) as u8) | 0x7E | ((ext >> 8) & 0x01) as u8;
        pkt[11] = (ext & 0xFF) as u8;
        let (pid, disc, pcr) = packet_pcr(&pkt, 0).expect("pcr");
        assert_eq!(pid, 0x0100);
        assert!(!disc);
        assert_eq!(pcr, base * 300 + ext);
    }

    #[test]
    fn packet_pcr_flags_discontinuity() {
        let mut pkt = vec![0u8; TS_UNIT];
        pkt[0] = SYNC;
        pkt[3] = 0x20; // adaptation field only
        pkt[4] = 7;
        pkt[5] = 0x90; // discontinuity_indicator + PCR_flag
        let (_, disc, _) = packet_pcr(&pkt, 0).expect("pcr");
        assert!(disc);
    }

    #[test]
    fn packet_pcr_absent_without_flag() {
        let mut pkt = vec![0u8; TS_UNIT];
        pkt[0] = SYNC;
        pkt[3] = 0x30; // adaptation field + payload
        pkt[4] = 7;
        pkt[5] = 0x00; // PCR_flag clear
        assert!(packet_pcr(&pkt, 0).is_none());
    }

    /// One 188-byte packet for `pid` carrying exactly `payload` bytes, the
    /// remainder stuffed via the adaptation field.
    fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0xFFu8; TS_UNIT];
        pkt[0] = SYNC;
        pkt[1] = ((pid >> 8) as u8 & 0x1F) | if pusi { 0x40 } else { 0x00 };
        pkt[2] = (pid & 0xFF) as u8;
        let body = TS_UNIT - 4;
        assert!(payload.len() <= body);
        if payload.len() == body {
            pkt[3] = 0x10; // payload only
            pkt[4..].copy_from_slice(payload);
        } else {
            pkt[3] = 0x30; // adaptation field (stuffing) + payload
            let af_len = body - payload.len() - 1;
            pkt[4] = af_len as u8;
            if af_len > 0 {
                pkt[5] = 0x00; // af flags; the rest stays 0xFF stuffing
            }
            let start = 5 + af_len;
            pkt[start..start + payload.len()].copy_from_slice(payload);
        }
        pkt
    }

    /// A PES start whose ES payload is `es` (header_data_length = 0).
    fn pes_start(es: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00];
        v.extend_from_slice(es);
        v
    }

    /// Two interleaved PIDs; AUs span packets; both PIDs end mid-AU, so the
    /// trailing accumulators must never be flushed. Completed AUs in emission
    /// order: pid 0x100 `[1,2,3,4,5]`, pid 0x200 `[9,9,8]`, pid 0x100 `[6]`.
    fn dual_pid_stream() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend(ts_packet(0x100, true, &pes_start(&[1, 2, 3])));
        d.extend(ts_packet(0x200, true, &pes_start(&[9, 9])));
        d.extend(ts_packet(0x100, false, &[4, 5]));
        d.extend(ts_packet(0x200, false, &[8]));
        d.extend(ts_packet(0x100, true, &pes_start(&[6]))); // completes [1,2,3,4,5]
        d.extend(ts_packet(0x200, true, &pes_start(&[7, 7]))); // completes [9,9,8]
        d.extend(ts_packet(0x100, true, &pes_start(&[0xAB]))); // completes [6]
        d.extend(ts_packet(0x200, false, &[0xCD])); // partials [0xAB] and [7,7,0xCD] never flush
        d
    }

    #[test]
    fn streamed_windows_match_one_shot() {
        let d = dual_pid_stream();
        let layout = Layout { first: 0, stride: TS_UNIT };
        let pids = [0x100u16, 0x200];

        let one_group = vec![pids.to_vec()];
        let mut one = vec![EsOut::default()];
        let mut st = EsStreamer::new(layout, &one_group, usize::MAX);
        assert!(!st.next_window(&d, &mut one, usize::MAX));
        assert_eq!(one[0].buf, [1, 2, 3, 4, 5, 9, 9, 8, 6]);
        assert_eq!(one[0].chunks.iter().map(|c| c.size).collect::<Vec<_>>(), [5, 3, 1]);

        // Tiny windows: identical bytes, AU sequence, and byte total. The pause
        // after the first emission leaves pid 0x200's AU half-accumulated, so a
        // partial AU straddling a window boundary is carried and emitted intact.
        let mut st = EsStreamer::new(layout, &one_group, usize::MAX);
        let mut outs = vec![EsOut::default()];
        let mut all = Vec::new();
        let mut sizes = Vec::new();
        let mut total = 0u64;
        let mut positions = Vec::new();
        loop {
            outs[0].clear();
            let more = st.next_window(&d, &mut outs, 1);
            positions.push(st.position());
            all.extend_from_slice(&outs[0].buf);
            sizes.extend(outs[0].chunks.iter().map(|c| c.size));
            total += outs[0].buf.len() as u64;
            if !more {
                break;
            }
        }
        assert_eq!(all, one[0].buf);
        assert_eq!(sizes, [5, 3, 1]);
        assert_eq!(total, one[0].buf.len() as u64);
        // The progress cursor is monotonic and ends at the walk's end.
        assert!(positions.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(*positions.last().unwrap(), d.len());
    }

    #[test]
    fn streamer_routes_per_group() {
        // The same dual-PID stream split into two independent groups: each
        // PID's completed AUs land in its own buffer, partials never flush.
        let d = dual_pid_stream();
        let layout = Layout { first: 0, stride: TS_UNIT };
        let groups = vec![vec![0x100u16], vec![0x200u16]];
        let mut outs = vec![EsOut::default(), EsOut::default()];
        let mut st = EsStreamer::new(layout, &groups, usize::MAX);
        assert!(!st.next_window(&d, &mut outs, usize::MAX));
        assert_eq!(outs[0].buf, [1, 2, 3, 4, 5, 6]);
        assert_eq!(outs[0].chunks.iter().map(|c| c.size).collect::<Vec<_>>(), [5, 1]);
        assert_eq!(outs[1].buf, [9, 9, 8]);
        assert_eq!(outs[1].chunks.iter().map(|c| c.size).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn full_demux_keeps_head_and_exposes_plan() {
        // PAT (program 1 -> PMT 0x0100) + the 2-stream PMT from
        // `pmt_parses_hevc_and_dovi_streams` + three video packets forming one
        // completed AU and a trailing partial.
        let pat: [u8; 16] = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, // header
            0x00, 0x01, 0xE1, 0x00, // program 1 -> 0x0100
            0x00, 0x00, 0x00, 0x00, // CRC
        ];
        let pmt: [u8; 35] = [
            0x02, 0xB0, 0x20, 0x00, 0x01, 0xC1, 0x00, 0x00, // table header
            0xF0, 0x11, 0xF0, 0x00, // PCR PID 0x1011, program_info_length 0
            0x24, 0xF0, 0x11, 0xF0, 0x00, // ES1: HEVC PID 0x1011
            0x06, 0xF0, 0x15, 0xF0, 0x09, // ES2: private PID 0x1015, es_info 9
            0xB0, 0x07, 0x01, 0x00, 0x0E, 0x36, 0x80, 0x8F, 0x6F, // DV descriptor
            0x00, 0x00, 0x00, 0x00, // CRC
        ];
        let mut pat_payload = vec![0x00]; // pointer_field
        pat_payload.extend_from_slice(&pat);
        let mut pmt_payload = vec![0x00];
        pmt_payload.extend_from_slice(&pmt);
        let mut d = Vec::new();
        d.extend(ts_packet(PID_PAT, true, &pat_payload));
        d.extend(ts_packet(0x0100, true, &pmt_payload));
        d.extend(ts_packet(0x1011, true, &pes_start(&[0x11, 0x22])));
        d.extend(ts_packet(0x1011, true, &pes_start(&[0x33]))); // completes [0x11,0x22]
        d.extend(ts_packet(0x1011, false, &[0x44])); // trailing partial

        let default = demux(&d, false, &Progress::off(), &Frontier::off()).expect("default demux");
        assert!(default.ts_stream.is_none());

        let full = demux(&d, true, &Progress::off(), &Frontier::off()).expect("full demux");
        let plan = full.ts_stream.as_ref().expect("full exposes the streaming plan");
        // BL + its dependent EL stay one group: one logical track.
        assert_eq!(plan.groups, [[0x1011, 0x1015]]);
        assert_eq!(full.tracks.len(), 1);
        let (ft, dt) = (&full.tracks[0], &default.tracks[0]);
        assert!(ft.dv_dual_track, "descriptor-flagged EL PID means dual track");
        assert_eq!(ft.track_number, Some(0x1011), "identity is the BL PID");
        assert_eq!(ft.program, None, "single-program mux omits the program");
        assert!(ft.bitrate.is_none(), "the streaming scan supplies the full-path rate");
        // The metadata pass is the same bounded head reassembly either way.
        assert_eq!(ft.reassembled, dt.reassembled);
        assert_eq!(ft.chunks.len(), 1);
    }

    /// An `Es` for grouping tests.
    fn es(pid: u16, stream_type: u8, dv: Option<(bool, Option<u16>)>) -> Es {
        let (has_dovi, dv_config, dependency_pid) = match dv {
            Some((bl_present, dep)) => (
                true,
                Some(DvConfig {
                    profile: 7,
                    level: Some(6),
                    bl_present,
                    el_present: true,
                    rpu_present: true,
                    bl_compatibility_id: Some(6),
                }),
                dep,
            ),
            None => (false, None, None),
        };
        Es { pid, stream_type, has_dovi, dv_config, dependency_pid }
    }

    fn prog(n: u16, streams: Vec<Es>) -> Program {
        Program { program_number: n, pcr_pid: PID_NONE, streams }
    }

    #[test]
    fn grouping_folds_el_by_dependency_pid() {
        // Two independent BLs + an EL whose dependency_pid names the *second*
        // BL: the EL folds there, not into the first.
        let g = group_video_pids(&[prog(
            1,
            vec![
                es(0x100, STREAM_TYPE_HEVC, None),
                es(0x200, STREAM_TYPE_HEVC, None),
                es(0x300, 0x06, Some((false, Some(0x200)))),
            ],
        )]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].pids(), [0x100]);
        assert!(!g[0].dv_dual_track);
        assert_eq!(g[1].pids(), [0x200, 0x300]);
        assert!(g[1].dv_dual_track);
        assert_eq!(g[1].primary_pid(), 0x200);
    }

    #[test]
    fn grouping_independent_video_pid_is_not_misread_as_el() {
        // A single-layer DV PID (bl_present=1) plus an unrelated second video
        // PID: two independent tracks — the old ">1 video PID means dual
        // track" rule must not fire when descriptors say otherwise.
        let g = group_video_pids(&[prog(
            1,
            vec![
                es(0x100, STREAM_TYPE_HEVC, Some((true, None))),
                es(0x200, STREAM_TYPE_HEVC, None),
            ],
        )]);
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|g| !g.dv_dual_track));
    }

    #[test]
    fn grouping_descriptorless_multi_pid_keeps_bdmv_rule() {
        // No DV descriptor anywhere (an untouched BDMV carries none): more
        // than one video PID is the Blu-ray P7 BL+EL pair — one dual group.
        let g = group_video_pids(&[prog(
            1,
            vec![es(0x1011, STREAM_TYPE_HEVC, None), es(0x1015, STREAM_TYPE_HEVC, None)],
        )]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].pids(), [0x1011, 0x1015]);
        assert!(g[0].dv_dual_track);
        assert_eq!(g[0].primary_pid(), 0x1011);
    }

    #[test]
    fn grouping_spans_programs() {
        // A whole-mux capture: one video PID per program → one track per
        // program, in program order; a bare EL-only program reports as-is.
        let g = group_video_pids(&[
            prog(1, vec![es(0x100, STREAM_TYPE_HEVC, None)]),
            prog(2, vec![es(0x200, STREAM_TYPE_AVC, None)]),
            prog(3, vec![es(0x300, 0x06, Some((false, None)))]),
        ]);
        assert_eq!(g.len(), 3);
        assert_eq!(
            g.iter().map(|g| (g.program_number, g.primary_pid())).collect::<Vec<_>>(),
            [(1, 0x100), (2, 0x200), (3, 0x300)]
        );
        assert!(g.iter().all(|g| !g.dv_dual_track));
        assert!(matches!(group_codec(&g[1].streams), Codec::Avc));
    }

    #[test]
    fn multi_program_demux_reports_one_track_per_program() {
        // PAT with two programs → two PMTs → interleaved video packets on two
        // PIDs: two tracks, each with its own program number, reassembly
        // buffer, and no cross-pollution.
        let pat: [u8; 20] = [
            0x00, 0xB0, 0x11, 0x00, 0x01, 0xC1, 0x00, 0x00, // header (len 0x11)
            0x00, 0x01, 0xE1, 0x00, // program 1 -> PMT 0x0100
            0x00, 0x02, 0xE2, 0x00, // program 2 -> PMT 0x0200
            0x00, 0x00, 0x00, 0x00, // CRC
        ];
        let pmt = |video_pid: u16| -> Vec<u8> {
            vec![
                0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00, // table header
                0xFF, 0xFF, 0xF0, 0x00, // PCR PID 0x1FFF (none), no prog info
                0x24, 0xE0 | (video_pid >> 8) as u8, (video_pid & 0xFF) as u8, 0xF0, 0x00,
                0x00, 0x00, 0x00, 0x00, // CRC
            ]
        };
        let with_ptr = |s: &[u8]| {
            let mut v = vec![0x00];
            v.extend_from_slice(s);
            v
        };
        let mut d = Vec::new();
        d.extend(ts_packet(PID_PAT, true, &with_ptr(&pat)));
        d.extend(ts_packet(0x0100, true, &with_ptr(&pmt(0x1011))));
        d.extend(ts_packet(0x0200, true, &with_ptr(&pmt(0x1211))));
        d.extend(ts_packet(0x1011, true, &pes_start(&[0x11, 0x22])));
        d.extend(ts_packet(0x1211, true, &pes_start(&[0x33])));
        d.extend(ts_packet(0x1011, true, &pes_start(&[0x44]))); // completes [0x11,0x22]
        d.extend(ts_packet(0x1211, true, &pes_start(&[0x55]))); // completes [0x33]

        let dm = demux(&d, false, &Progress::off(), &Frontier::off()).expect("demux");
        assert_eq!(dm.tracks.len(), 2);
        let (t1, t2) = (&dm.tracks[0], &dm.tracks[1]);
        assert_eq!((t1.track_number, t1.program), (Some(0x1011), Some(1)));
        assert_eq!((t2.track_number, t2.program), (Some(0x1211), Some(2)));
        assert_eq!(t1.reassembled.as_deref(), Some(&[0x11u8, 0x22][..]));
        assert_eq!(t2.reassembled.as_deref(), Some(&[0x33u8][..]));
        assert_eq!(t1.chunks.len(), 1);
        assert_eq!(t2.chunks.len(), 1);
        // Multi-track: no overall bitrate attributed to either track.
        assert!(t1.bitrate.is_none() && t2.bitrate.is_none());
    }

    #[test]
    fn reassembly_emits_au_on_next_pes() {
        // A PES spanning two packets is emitted only when the next PES starts.
        let mut st = PidState { pid: 0x100, group: 0, acc: Vec::new(), started: false };
        let mut out = EsOut::default();
        let pes1 = [0x00, 0x00, 0x01, 0xE0, 0, 0, 0x80, 0, 0, 0xAA, 0xBB, 0xCC];
        process(&mut st, true, &pes1, &mut out);
        process(&mut st, false, &[0xDD], &mut out);
        assert!(out.chunks.is_empty()); // AU not yet complete
        let pes2 = [0x00, 0x00, 0x01, 0xE0, 0, 0, 0x80, 0, 0, 0x11];
        let appended = process(&mut st, true, &pes2, &mut out);
        assert_eq!(out.chunks.len(), 1);
        assert_eq!(appended, 4);
        let c = out.chunks[0];
        assert_eq!(
            &out.buf[c.offset as usize..(c.offset + c.size) as usize],
            &[0xAA, 0xBB, 0xCC, 0xDD]
        );
    }
}
