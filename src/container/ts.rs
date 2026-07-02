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
//! Annex-B buffer to the normal NAL walker for RPU/SEI extraction.
//!
//! **Duration** has no container box in TS, so — like MediaInfo — we derive it
//! from the transport clock: `last_PCR - first_PCR` on the PCR PID (a 27 MHz
//! reference carried in adaptation fields at least every 100 ms). The first PCR
//! is free from the head window we already read; the last comes from a small
//! bounded tail window (`TAIL_SCAN_BYTES`), which the network prefetch warms in
//! one read alongside the head. The middle of a multi-GB stream is never touched.

use anyhow::{bail, Context, Result};

use crate::container::{Chunk, Codec, Demux, DvConfig, NalFormat};
use crate::hevc::nal::{self, NalRef};
use crate::hevc::sps::{parse_sps, SpsInfo};
use crate::model::{Bitrate, ColorInfo};

const SYNC: u8 = 0x47;
const TS_UNIT: usize = 188;
const PID_PAT: u16 = 0x0000;
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

/// Reassembly limits. TS titles are large (multi-GB 4K discs), so by default we
/// peek at a bounded window **at the head only** (`HEAD_SCAN_BYTES`) — enough to
/// reassemble the SPS and confirm the RPU/DV levels, then bail — never spreading
/// across the file. This keeps the fast path to a single front-of-file region
/// (which the network prefetch warms in one read; see `prefetch::warm_metadata`)
/// and mirrors the head-bounded MKV/raw-HEVC default. `--full` lifts every cap for
/// a true exhaustive scan (reassembling the whole video stream, no 2s guarantee,
/// and picking up dynamic per-shot variation the default deliberately skips).
struct Limits {
    windows: usize,
    aus_per_window: usize,
    window_packet_budget: usize,
    pid_au_cap: usize,
    pid_byte_cap: usize,
}

impl Limits {
    fn sampled() -> Self {
        Limits {
            // A single head window bounded only by a byte budget: the default just
            // grabs title-stable static metadata, so there is no reason to fault in
            // packets spread across a multi-GB file. The AU/byte caps are lifted so
            // the read isn't cut short before the first IDR/SPS; the packet budget
            // (sized to `HEAD_SCAN_BYTES`, the prefetch-warmed region) is the sole
            // bound. Divide by the larger stride (192) so the byte span read stays
            // within `HEAD_SCAN_BYTES` for both TS (188) and M2TS (192).
            windows: 1,
            aus_per_window: usize::MAX,
            window_packet_budget: (HEAD_SCAN_BYTES / 192) as usize,
            pid_au_cap: usize::MAX,
            pid_byte_cap: usize::MAX,
        }
    }
    /// A single window spanning the whole file with every cap removed.
    fn full() -> Self {
        Limits {
            windows: 1,
            aus_per_window: usize::MAX,
            window_packet_budget: usize::MAX,
            pid_au_cap: usize::MAX,
            pid_byte_cap: usize::MAX,
        }
    }
}

/// Packet layout: offset of the first sync byte and the packet stride.
#[derive(Clone, Copy)]
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

pub fn demux(data: &[u8], full: bool) -> Result<Demux> {
    let layout = detect_layout(data).context("not a recognized TS/M2TS stream")?;
    let (pcr_pid, streams) = parse_psi(data, layout).context("no PMT / program map found")?;

    // Video PIDs: standard HEVC streams plus any PID tagged as Dolby Vision
    // (a Profile-7 enhancement layer rides its own PID with a DV descriptor).
    let video_pids: Vec<u16> = streams
        .iter()
        .filter(|e| e.stream_type == STREAM_TYPE_HEVC || e.has_dovi)
        .map(|e| e.pid)
        .collect();
    if video_pids.is_empty() {
        bail!("no HEVC/Dolby Vision video PID in the program map");
    }
    let dv_config = streams.iter().find_map(|e| e.dv_config.clone());

    let limits = if full { Limits::full() } else { Limits::sampled() };
    let (buf, chunks) = reassemble(data, layout, &video_pids, &limits);
    let (width, height, bit_depth, chroma, codec_profile, color, fps) = sps_metadata(&buf, &chunks);

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

    // Only `--full` reassembles the entire elementary stream; then `buf` is the
    // exact video-stream byte count. The default reassembles just a head window,
    // so report the container's overall rate from the file length instead.
    let bitrate = if full {
        Bitrate::video_stream(buf.len() as u64, duration_secs)
    } else {
        Bitrate::overall(data.len() as u64, duration_secs)
    };

    Ok(Demux {
        container,
        codec: Codec::Hevc,
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
        reassembled: Some(buf),
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
                cfg = super::parse_dovi_config(body).or(cfg);
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
    au_total: usize,
    bytes_total: usize,
    win_emitted: usize,
}

impl PidState {
    fn quota_reached(&self, lim: &Limits) -> bool {
        self.au_total >= lim.pid_au_cap || self.bytes_total >= lim.pid_byte_cap
    }
    fn done_for_window(&self, lim: &Limits) -> bool {
        self.win_emitted >= lim.aus_per_window || self.quota_reached(lim)
    }
}

/// Reassemble access units for the target PIDs into one owned Annex-B buffer,
/// returning it with per-AU chunk ranges into it. Sampling breadth is bounded by
/// `lim` (a single bounded head window by default, the whole stream under `--full`).
fn reassemble(data: &[u8], layout: Layout, pids: &[u16], lim: &Limits) -> (Vec<u8>, Vec<Chunk>) {
    let mut states: Vec<PidState> = pids
        .iter()
        .map(|&pid| PidState {
            pid,
            acc: Vec::new(),
            started: false,
            au_total: 0,
            bytes_total: 0,
            win_emitted: 0,
        })
        .collect();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunks: Vec<Chunk> = Vec::new();

    // Disjoint windows: each is bounded by the next window's start so a small
    // file is covered exactly once (no duplicate AUs), while a large file only
    // reads a bounded budget of each window.
    let starts = window_starts(data, layout, lim.windows);
    for (wi, &ws) in starts.iter().enumerate() {
        let win_end = starts.get(wi + 1).copied().unwrap_or(data.len()).min(data.len());
        let Some(mut p) = align(data, layout, ws) else { continue };
        for s in states.iter_mut() {
            s.acc.clear();
            s.started = false;
            s.win_emitted = 0;
        }
        let mut packets = 0;
        while p + TS_UNIT <= win_end && packets < lim.window_packet_budget {
            if data[p] != SYNC {
                match resync(data, layout, p) {
                    Some(np) => {
                        p = np;
                        continue;
                    }
                    None => break,
                }
            }
            if let Some((pid, pusi, payload)) = packet_payload(data, p) {
                if let Some(si) = states.iter().position(|s| s.pid == pid) {
                    process(&mut states[si], pusi, payload, &mut buf, &mut chunks, lim);
                }
            }
            if states.iter().all(|s| s.done_for_window(lim)) {
                break;
            }
            p += layout.stride;
            packets += 1;
        }
    }
    (buf, chunks)
}

/// Feed one packet's payload into a PID's reassembler, emitting a completed
/// access unit (into `buf`/`chunks`) when a new PES starts.
fn process(
    st: &mut PidState,
    pusi: bool,
    payload: &[u8],
    buf: &mut Vec<u8>,
    chunks: &mut Vec<Chunk>,
    lim: &Limits,
) {
    if st.done_for_window(lim) {
        return;
    }
    if pusi {
        // A new PES begins: finalize the previous access unit.
        if st.started && !st.acc.is_empty() {
            let offset = buf.len() as u64;
            buf.extend_from_slice(&st.acc);
            chunks.push(Chunk { offset, size: st.acc.len() as u64 });
            st.au_total += 1;
            st.bytes_total += st.acc.len();
            st.win_emitted += 1;
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

/// Byte offsets to begin each reassembly window: the true stream head plus an
/// even spread across the file.
fn window_starts(data: &[u8], layout: Layout, windows: usize) -> Vec<usize> {
    let windows = windows.max(1);
    let mut v = Vec::with_capacity(windows);
    v.push(layout.first);
    let len = data.len();
    for w in 1..windows {
        v.push(len * w / windows);
    }
    v.sort_unstable();
    v.dedup();
    v
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

/// Recover resolution / bit depth / chroma / colour / frame rate from the widest
/// SPS in the reassembled buffer (the base layer outranks a smaller enhancement
/// layer). TS carries no container timing box, so the frame rate — like the
/// colour — comes only from the SPS VUI.
#[allow(clippy::type_complexity)]
fn sps_metadata(
    buf: &[u8],
    chunks: &[Chunk],
) -> (u32, u32, Option<u8>, Option<String>, Option<String>, ColorInfo, Option<f64>) {
    let mut best: Option<SpsInfo> = None;
    let mut nals: Vec<NalRef> = Vec::new();
    for c in chunks {
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
                    if best.as_ref().is_none_or(|b| sps.width > b.width) {
                        best = Some(sps);
                    }
                }
            }
        }
        if best.as_ref().is_some_and(|b| b.width >= 3840) {
            break;
        }
    }
    match best {
        Some(sps) => {
            let color = sps
                .color
                .as_ref()
                .map(crate::container::color_from_vui)
                .unwrap_or_default();
            (
                sps.width,
                sps.height,
                Some(sps.bit_depth),
                Some(sps.chroma_str().to_string()),
                Some(sps.profile_label()),
                color,
                sps.frame_rate,
            )
        }
        None => (0, 0, None, None, None, ColorInfo::default(), None),
    }
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
        // tag-0xB0 Dolby Vision descriptor whose body is a dvcC-shaped record:
        // profile 7, level 6, rpu+el present, bl absent, compat 8.
        let s: [u8; 33] = [
            0x02, 0xB0, 0x1E, 0x00, 0x01, 0xC1, 0x00, 0x00, // table header
            0xF0, 0x11, 0xF0, 0x00, // PCR PID 0x1011, program_info_length 0
            0x24, 0xF0, 0x11, 0xF0, 0x00, // ES1: HEVC PID 0x1011
            0x06, 0xF0, 0x15, 0xF0, 0x07, // ES2: private PID 0x1015, es_info 7
            0xB0, 0x05, 0x01, 0x00, 0x0E, 0x36, 0x80, // DV descriptor
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
        assert_eq!(cfg.bl_compatibility_id, Some(8));
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

    #[test]
    fn reassembly_emits_au_on_next_pes() {
        // A PES spanning two packets is emitted only when the next PES starts.
        let mut st = PidState {
            pid: 0x100,
            acc: Vec::new(),
            started: false,
            au_total: 0,
            bytes_total: 0,
            win_emitted: 0,
        };
        let mut buf = Vec::new();
        let mut chunks = Vec::new();
        let lim = Limits::sampled();
        let pes1 = [0x00, 0x00, 0x01, 0xE0, 0, 0, 0x80, 0, 0, 0xAA, 0xBB, 0xCC];
        process(&mut st, true, &pes1, &mut buf, &mut chunks, &lim);
        process(&mut st, false, &[0xDD], &mut buf, &mut chunks, &lim);
        assert!(chunks.is_empty()); // AU not yet complete
        let pes2 = [0x00, 0x00, 0x01, 0xE0, 0, 0, 0x80, 0, 0, 0x11];
        process(&mut st, true, &pes2, &mut buf, &mut chunks, &lim);
        assert_eq!(chunks.len(), 1);
        let c = chunks[0];
        assert_eq!(&buf[c.offset as usize..(c.offset + c.size) as usize], &[0xAA, 0xBB, 0xCC, 0xDD]);
    }
}
