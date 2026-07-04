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
use crate::container::{Chunk, Codec, Demux, DvConfig, NalFormat};
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
    let (pcr_pid, streams) = parse_psi(data, layout).context("no PMT / program map found")?;

    // Video PIDs: standard HEVC/AVC streams plus any PID tagged as Dolby Vision
    // (a Profile-7 enhancement layer rides its own PID with a DV descriptor).
    let video_pids: Vec<u16> = streams
        .iter()
        .filter(|e| e.stream_type == STREAM_TYPE_HEVC || e.stream_type == STREAM_TYPE_AVC || e.has_dovi)
        .map(|e| e.pid)
        .collect();
    if video_pids.is_empty() {
        bail!("no HEVC/AVC/Dolby Vision video PID in the program map");
    }
    let dv_config = streams.iter().find_map(|e| e.dv_config.clone());

    // Codec of the base layer: the PMT stream_type is authoritative (0x1B AVC,
    // 0x24 HEVC). A DV-only PID (EL, PES-private 0x06) carries no video type, so
    // fall back to the DV profile — only profile 9 is AVC. HEVC wins a tie (an
    // AVC EL alongside an HEVC BL is not a real configuration, but be explicit).
    let has_type = |t: u8| streams.iter().any(|e| video_pids.contains(&e.pid) && e.stream_type == t);
    let codec = if has_type(STREAM_TYPE_HEVC) {
        Codec::Hevc
    } else if has_type(STREAM_TYPE_AVC) || dv_config.as_ref().map(|c| c.profile) == Some(9) {
        Codec::Avc
    } else {
        Codec::Hevc
    };

    // Metadata always comes from the bounded head pass — even under `--full`,
    // which streams the whole elementary stream through `sample::scan` in
    // bounded windows (`TsFullStream`) instead of materializing it here.
    let (buf, chunks) = head_reassemble(data, layout, &video_pids);

    let mut best = best_sps(&buf, &chunks, &codec);
    let mut sps_chunk = best.as_ref().map(|c| c.chunk);
    if full && best.is_none() {
        // The head window held no SPS at all (first IDR beyond
        // `HEAD_SCAN_BYTES` — atypical captures): under `--full` keep looking
        // through the whole stream rather than losing the resolution the old
        // whole-stream pass would have found. A rescue hit's chunk index is
        // window-relative, meaningless against the head chunks — and unneeded:
        // the `--full` scan covers every AU, so nothing must be pinned.
        best = sps_rescue(data, layout, &video_pids, &codec, progress, frontier);
        sps_chunk = None;
    }
    let (width, height, bit_depth, chroma, codec_profile, color, fps) = sps_fields(best);

    // Duration from the transport clock (head+tail PCR delta). Prefer the PMT's
    // declared PCR PID, falling back to the video PID(s) — most streams carry the
    // PCR on the video PID anyway.
    let duration_secs = std::iter::once(pcr_pid)
        .chain(video_pids.iter().copied())
        .filter(|&pid| pid != PID_NONE)
        .find_map(|pid| pcr_duration(data, layout, pid));

    let container = if layout.stride == 192 {
        "MPEG-2 TS (M2TS/BDAV)"
    } else {
        "MPEG-2 TS"
    };

    // `--full`: the exact video-stream byte total is only known after the
    // sampler's streaming walk, so leave the rate unset here — main.rs fills it
    // from `sample::Scan::es_bytes` (the same value the old whole-stream
    // reassembly produced, including `None` when no video bytes complete). The
    // default bounded path reports the file-length overall rate as before.
    let bitrate = if full { None } else { Bitrate::overall(data.len() as u64, duration_secs) };

    let ts_stream = full.then(|| TsFullStream { layout, video_pids: video_pids.clone() });

    Ok(Demux {
        container,
        codec,
        nal_format: NalFormat::AnnexB,
        width,
        height,
        fps,
        duration_secs,
        bit_depth,
        chroma,
        codec_profile,
        stereo: None,
        color,
        dv_config,
        // A Profile-7 EL rides its own PID, so more than one video PID means the
        // base and enhancement layers are on separate streams (dual track).
        dv_dual_track: video_pids.len() > 1,
        mastering: None,
        content_light: None,
        bitrate,
        chunks,
        sps_chunk,
        reassembled: Some(buf),
        ts_stream,
    })
}

// --- PSI (PAT / PMT) --------------------------------------------------------

/// One elementary stream from the PMT.
struct Es {
    pid: u16,
    stream_type: u8,
    has_dovi: bool,
    dv_config: Option<DvConfig>,
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

/// Walk the head of the stream to resolve the PMT PID (from the PAT) and then
/// the PCR PID and elementary streams (from the PMT).
fn parse_psi(data: &[u8], layout: Layout) -> Option<(u16, Vec<Es>)> {
    let mut pmt_pid: Option<u16> = None;
    let mut p = layout.first;
    let mut scanned = 0;
    while p + TS_UNIT <= data.len() && scanned < 20_000 {
        if data[p] == SYNC {
            if let Some((pid, pusi, payload)) = packet_payload(data, p) {
                if pid == PID_PAT && pusi {
                    if let Some(mp) = parse_pat(payload) {
                        pmt_pid = Some(mp);
                        break;
                    }
                }
            }
        }
        p += layout.stride;
        scanned += 1;
    }
    let pmt_pid = pmt_pid?;

    let mut p = layout.first;
    let mut scanned = 0;
    while p + TS_UNIT <= data.len() && scanned < 40_000 {
        if data[p] == SYNC {
            if let Some((pid, pusi, payload)) = packet_payload(data, p) {
                if pid == pmt_pid && pusi {
                    if let Some(parsed) = parse_pmt(payload) {
                        return Some(parsed);
                    }
                }
            }
        }
        p += layout.stride;
        scanned += 1;
    }
    None
}

/// PAT: return the first program's `program_map_PID`.
fn parse_pat(payload: &[u8]) -> Option<u16> {
    let ptr = *payload.first()? as usize;
    let s = payload.get(1 + ptr..)?;
    if *s.first()? != 0x00 {
        return None; // table_id
    }
    let section_length = (((s.get(1)? & 0x0F) as usize) << 8) | *s.get(2)? as usize;
    let prog_end = (3 + section_length).saturating_sub(4).min(s.len()); // exclude CRC
    let mut i = 8;
    while i + 4 <= prog_end {
        let prog_num = ((s[i] as u16) << 8) | s[i + 1] as u16;
        let map_pid = (((s[i + 2] & 0x1F) as u16) << 8) | s[i + 3] as u16;
        if prog_num != 0 {
            return Some(map_pid);
        }
        i += 4;
    }
    None
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
        let (has_dovi, dv_config) = scan_descriptors(desc);
        streams.push(Es { pid, stream_type, has_dovi, dv_config });
        i += 5 + es_info_len;
    }
    Some((pcr_pid, streams))
}

/// Scan ES-info descriptors for Dolby Vision signalling: a `DOVI` registration
/// descriptor (tag 0x05) and/or the DV video-stream descriptor (tag 0xB0, whose
/// body is a `dvcC`-shaped config record).
fn scan_descriptors(d: &[u8]) -> (bool, Option<DvConfig>) {
    let mut has_dovi = false;
    let mut cfg = None;
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
                cfg = super::parse_dovi_ts_descriptor(body).or(cfg);
            }
            _ => {}
        }
        i += 2 + len;
    }
    (has_dovi, cfg)
}

// --- PES reassembly ---------------------------------------------------------

struct PidState {
    pid: u16,
    acc: Vec<u8>,
    started: bool,
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
    fn new(layout: Layout, pids: &[u16], packet_budget: usize) -> Self {
        EsStreamer {
            layout,
            states: pids
                .iter()
                .map(|&pid| PidState { pid, acc: Vec::new(), started: false })
                .collect(),
            cursor: layout.first,
            packets_left: packet_budget,
            finished: false,
        }
    }

    /// Append completed access units to `buf`/`chunks` (chunk offsets are
    /// relative to `buf`) until at least `target_bytes` accumulate or the walk
    /// ends (end of data, packet budget exhausted, or unrecoverable sync loss).
    /// A window may overshoot `target_bytes` by one AU — the whole accumulator
    /// is appended when a new PES start completes it. Returns `true` while more
    /// input remains.
    pub fn next_window(
        &mut self,
        data: &[u8],
        buf: &mut Vec<u8>,
        chunks: &mut Vec<Chunk>,
        target_bytes: usize,
    ) -> bool {
        if self.finished {
            return false;
        }
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
                    process(&mut self.states[si], pusi, payload, buf, chunks);
                }
            }
            self.cursor += self.layout.stride;
            self.packets_left -= 1;
            if buf.len() >= target_bytes {
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
/// single window bounded only by a packet budget sized to `HEAD_SCAN_BYTES`
/// (the prefetch-warmed region — divide by the larger stride, 192, so the byte
/// span stays within it for both TS and M2TS). The budget is the sole bound:
/// the default just grabs title-stable static metadata, and the read must not
/// be cut short before the first IDR/SPS, typically ~one 4K GOP in.
fn head_reassemble(data: &[u8], layout: Layout, pids: &[u16]) -> (Vec<u8>, Vec<Chunk>) {
    let mut buf = Vec::new();
    let mut chunks = Vec::new();
    let mut st = EsStreamer::new(layout, pids, (HEAD_SCAN_BYTES / 192) as usize);
    st.next_window(data, &mut buf, &mut chunks, usize::MAX);
    (buf, chunks)
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
    video_pids: Vec<u16>,
}

impl TsFullStream {
    #[cfg(test)]
    pub(crate) fn new(layout: Layout, video_pids: Vec<u16>) -> Self {
        TsFullStream { layout, video_pids }
    }

    /// A fresh unbounded streamer over the whole stream. It starts at the head,
    /// re-reading the window the metadata pass already parsed (<= 24 MiB, cheap)
    /// so every AU is scanned exactly once — by the streamer.
    pub fn streamer(&self) -> EsStreamer {
        EsStreamer::new(self.layout, &self.video_pids, usize::MAX)
    }
}

/// Feed one packet's payload into a PID's reassembler, emitting a completed
/// access unit (into `buf`/`chunks`) when a new PES starts.
fn process(st: &mut PidState, pusi: bool, payload: &[u8], buf: &mut Vec<u8>, chunks: &mut Vec<Chunk>) {
    if pusi {
        // A new PES begins: finalize the previous access unit.
        if st.started && !st.acc.is_empty() {
            let offset = buf.len() as u64;
            buf.extend_from_slice(&st.acc);
            chunks.push(Chunk { offset, size: st.acc.len() as u64 });
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
    pids: &[u16],
    codec: &Codec,
    progress: &Progress,
    frontier: &Frontier,
) -> Option<SpsCommon> {
    let mut st = EsStreamer::new(layout, pids, usize::MAX);
    let mut buf = Vec::new();
    let mut chunks = Vec::new();
    let mut best: Option<SpsCommon> = None;
    progress.begin(Phase::Index, data.len() as u64);
    // One window consumes more *file* bytes than its ES target (packet
    // overhead, other PIDs), so the frontier warms the upcoming window's file
    // span, adapted from the last window's observed density.
    let mut warm_span = STREAM_WINDOW_BYTES as u64 * 2;
    loop {
        buf.clear();
        chunks.clear();
        let pos0 = st.position() as u64;
        frontier.ensure_to(pos0.saturating_add(warm_span));
        let more = st.next_window(data, &mut buf, &mut chunks, STREAM_WINDOW_BYTES);
        let used = st.position() as u64 - pos0;
        if used > 0 {
            warm_span = used + used / 4;
        }
        if let Some(w) = best_sps(&buf, &chunks, codec) {
            if best.as_ref().is_none_or(|b| w.width > b.width) {
                best = Some(w);
            }
        }
        if !more || best.as_ref().is_some_and(|b| b.width >= 3840) {
            return best;
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
        assert_eq!(parse_pat(&payload), Some(0x0100));
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

        let mut one_buf = Vec::new();
        let mut one_chunks = Vec::new();
        let mut st = EsStreamer::new(layout, &pids, usize::MAX);
        assert!(!st.next_window(&d, &mut one_buf, &mut one_chunks, usize::MAX));
        assert_eq!(one_buf, [1, 2, 3, 4, 5, 9, 9, 8, 6]);
        assert_eq!(one_chunks.iter().map(|c| c.size).collect::<Vec<_>>(), [5, 3, 1]);

        // Tiny windows: identical bytes, AU sequence, and byte total. The pause
        // after the first emission leaves pid 0x200's AU half-accumulated, so a
        // partial AU straddling a window boundary is carried and emitted intact.
        let mut st = EsStreamer::new(layout, &pids, usize::MAX);
        let (mut buf, mut chunks) = (Vec::new(), Vec::new());
        let mut all = Vec::new();
        let mut sizes = Vec::new();
        let mut total = 0u64;
        let mut positions = Vec::new();
        loop {
            buf.clear();
            chunks.clear();
            let more = st.next_window(&d, &mut buf, &mut chunks, 1);
            positions.push(st.position());
            all.extend_from_slice(&buf);
            sizes.extend(chunks.iter().map(|c| c.size));
            total += buf.len() as u64;
            if !more {
                break;
            }
        }
        assert_eq!(all, one_buf);
        assert_eq!(sizes, [5, 3, 1]);
        assert_eq!(total, one_buf.len() as u64);
        // The progress cursor is monotonic and ends at the walk's end.
        assert!(positions.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(*positions.last().unwrap(), d.len());
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
        assert_eq!(plan.video_pids, [0x1011, 0x1015]);
        assert!(full.bitrate.is_none(), "the streaming scan supplies the full-path rate");
        // The metadata pass is the same bounded head reassembly either way.
        assert_eq!(full.reassembled, default.reassembled);
        assert_eq!(full.chunks.len(), 1);
    }

    #[test]
    fn reassembly_emits_au_on_next_pes() {
        // A PES spanning two packets is emitted only when the next PES starts.
        let mut st = PidState { pid: 0x100, acc: Vec::new(), started: false };
        let mut buf = Vec::new();
        let mut chunks = Vec::new();
        let pes1 = [0x00, 0x00, 0x01, 0xE0, 0, 0, 0x80, 0, 0, 0xAA, 0xBB, 0xCC];
        process(&mut st, true, &pes1, &mut buf, &mut chunks);
        process(&mut st, false, &[0xDD], &mut buf, &mut chunks);
        assert!(chunks.is_empty()); // AU not yet complete
        let pes2 = [0x00, 0x00, 0x01, 0xE0, 0, 0, 0x80, 0, 0, 0x11];
        process(&mut st, true, &pes2, &mut buf, &mut chunks);
        assert_eq!(chunks.len(), 1);
        let c = chunks[0];
        assert_eq!(&buf[c.offset as usize..(c.offset + c.size) as usize], &[0xAA, 0xBB, 0xCC, 0xDD]);
    }
}
