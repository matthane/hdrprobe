//! Raw AV1 elementary streams: IVF (`DKIF`) and low-overhead OBU (`.obu`).
//!
//! No container metadata — dimensions/bit-depth/colour come from the sequence
//! header OBU, and the Dolby Vision profile (always **10** for AV1) is inferred
//! from the presence of DV RPU metadata OBUs downstream. Demux-only.

use anyhow::{bail, Result};

use crate::av1::obu::{obus, OBU_SEQUENCE_HEADER, OBU_TEMPORAL_DELIMITER};
use crate::av1::seq::{parse_sequence_header, SeqInfo};
use crate::container::{Chunk, Codec, Demux, NalFormat};
use crate::model::ColorInfo;

const IVF_SIGNATURE: &[u8] = b"DKIF";

/// Default head-window budget for a raw AV1 stream. Everything the report needs —
/// the sequence header (dims/colour/fps), the first RPUs, static HDR — sits at the
/// front, and 8 MiB of a 4K stream is dozens of access units: enough to sample
/// without faulting the whole file. Bounded by default, exhaustive under `--full`.
///
/// Unlike Annex-B's `00 00 01` start codes, low-overhead OBU has **no byte-scannable
/// sync marker** — AV1 carries no emulation prevention, so a temporal-delimiter byte
/// pattern can occur inside frame payload — so we cannot spread windows across the
/// file the way `annexb` does. We walk one head window from the guaranteed byte-0
/// boundary, the same head-only shape TS uses to reach its in-band SPS. L5 is thus
/// sampled from the head (labelled `[sampled]`); `--full` walks every access unit.
///
/// **NAS coupling:** kept `<=` `prefetch::HEAD_WARM` so the warmed head covers the
/// whole walked span in one pipelined read — grow this without growing that and the
/// tail of the window faults in one page at a time again.
pub const HEAD_SCAN_BYTES: usize = 8 << 20; // 8 MiB

pub fn is_ivf(data: &[u8]) -> bool {
    data.len() >= 4 && &data[0..4] == IVF_SIGNATURE
}

/// True if the buffer plausibly begins with an AV1 low-overhead OBU stream: a
/// temporal delimiter or sequence header with the size-field flag set.
pub fn is_obu_stream(data: &[u8]) -> bool {
    if data.is_empty() || data[0] & 0x81 != 0 {
        return false; // forbidden_bit or reserved high bit set
    }
    let obu_type = (data[0] >> 3) & 0x0F;
    let has_size = data[0] & 0x02 != 0;
    has_size && matches!(obu_type, OBU_TEMPORAL_DELIMITER | OBU_SEQUENCE_HEADER)
}

pub fn demux(data: &[u8], full: bool) -> Result<Demux> {
    if is_ivf(data) {
        demux_ivf(data, full)
    } else if is_obu_stream(data) {
        demux_obu(data, "raw AV1 (OBU)", full)
    } else {
        bail!("not a recognized raw AV1 stream")
    }
}

/// IVF: 32-byte file header, then per-frame (4-byte LE size + 8-byte timestamp +
/// frame bytes). Each IVF frame is one temporal unit → one chunk. Bounded to a
/// head window by default (walk only up to `HEAD_SCAN_BYTES`), exhaustive under
/// `--full`.
fn demux_ivf(data: &[u8], full: bool) -> Result<Demux> {
    if data.len() < 32 {
        bail!("truncated IVF header");
    }
    let header_len = u16::from_le_bytes([data[6], data[7]]) as usize;
    let width = u16::from_le_bytes([data[12], data[13]]) as u32;
    let height = u16::from_le_bytes([data[14], data[15]]) as u32;
    // The IVF "rate/scale" is a time base (ticks/second), not a frame rate — the
    // real fps is recovered from the per-frame presentation timestamps below.
    let rate = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) as f64;
    let scale = u32::from_le_bytes([data[20], data[21], data[22], data[23]]).max(1) as f64;
    // The IVF header records the total frame count, so duration is exact even when
    // the frame walk is bounded to the head window. 0 = unfilled (some muxers).
    let header_frames = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);

    // Small streams are walked whole (cheaper than bounding); big ones stop at the
    // head window so cost is O(head) regardless of file size.
    let walked_all = full || data.len() <= HEAD_SCAN_BYTES;
    let scan_limit = if walked_all { data.len() } else { HEAD_SCAN_BYTES };

    let mut chunks = Vec::new();
    let mut pos = header_len.max(32);
    let mut first_ts: Option<u64> = None;
    let mut last_ts = 0u64;
    while pos + 12 <= data.len() {
        if pos >= scan_limit {
            break;
        }
        let size = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let ts = u64::from_le_bytes(data[pos + 4..pos + 12].try_into().unwrap());
        let frame_start = pos + 12;
        let frame_end = frame_start + size;
        if frame_end > data.len() {
            break;
        }
        first_ts.get_or_insert(ts);
        last_ts = ts;
        chunks.push(Chunk { offset: frame_start as u64, size: size as u64 });
        pos = frame_end;
    }

    // fps = ticks/sec ÷ (mean ticks per frame) = (rate/scale)·(frames−1)/timespan.
    // Constant-rate streams give the same answer over the head window as the whole
    // file, so bounding the walk doesn't change it.
    let ticks_per_sec = rate / scale;
    let span = last_ts.saturating_sub(first_ts.unwrap_or(0));
    let fps = match (chunks.len(), span) {
        (n, s) if n > 1 && s > 0 => Some(ticks_per_sec * (n as f64 - 1.0) / s as f64),
        _ => None,
    }
    .filter(|&f| f > 0.0 && f <= 480.0)
    .map(snap_to_standard_fps);

    let seq = chunks.iter().find_map(|c| find_seq_header(&data[c.offset as usize..(c.offset + c.size) as usize]));
    // Exact frame count: the whole-file chunk count when we walked it all, else the
    // header's total (when the muxer filled it) so duration survives the bounded walk.
    let frame_count = if walked_all {
        (!chunks.is_empty()).then_some(chunks.len() as u64)
    } else {
        (header_frames > 0).then_some(header_frames as u64)
    };
    Ok(build_demux("raw AV1 (IVF)", width, height, fps, frame_count, seq, chunks))
}

fn demux_obu(data: &[u8], label: &'static str, full: bool) -> Result<Demux> {
    // Bounded head window by default (from the byte-0 boundary — OBU has no
    // resync marker), whole stream under `--full` or when it fits the window.
    let walked_all = full || data.len() <= HEAD_SCAN_BYTES;
    let scan = if walked_all { data } else { &data[..HEAD_SCAN_BYTES] };
    let (chunks, frame_count) = split_obu_temporal_units(scan, walked_all);
    let seq = find_seq_header(scan);
    let (w, h) = seq.as_ref().map(|s| (s.width, s.height)).unwrap_or((0, 0));
    // The low-overhead OBU stream carries no timestamps, so a frame rate exists
    // only when the sequence header signals constant `timing_info()`.
    let fps = seq.as_ref().and_then(|s| s.fps);
    Ok(build_demux(label, w, h, fps, frame_count, seq, chunks))
}

/// Split a raw low-overhead OBU stream into temporal units (each starting at an
/// `OBU_TEMPORAL_DELIMITER`) so downstream sampling has real access units. `data`
/// is the byte range to walk — the whole stream under `--full`, else the head
/// window. The boundary count is the exact frame count **only when the whole
/// stream was walked** (`walked_all`); a bounded head window sees only a prefix,
/// so `frame_count` is `None` there (and duration with it). Also `None` in the
/// no-delimiter fallback, where the single whole-buffer chunk is not a frame count.
fn split_obu_temporal_units(data: &[u8], walked_all: bool) -> (Vec<Chunk>, Option<u64>) {
    let mut boundaries = Vec::new();
    for obu in obus(data) {
        if obu.obu_type == OBU_TEMPORAL_DELIMITER {
            boundaries.push(obu.start);
        }
    }
    if boundaries.is_empty() {
        return (vec![Chunk { offset: 0, size: data.len() as u64 }], None);
    }
    let mut chunks = Vec::with_capacity(boundaries.len());
    for i in 0..boundaries.len() {
        let start = boundaries[i];
        let end = boundaries.get(i + 1).copied().unwrap_or(data.len());
        chunks.push(Chunk { offset: start as u64, size: (end - start) as u64 });
    }
    let frame_count = walked_all.then_some(boundaries.len() as u64);
    (chunks, frame_count)
}

/// Broadcast/film frame rates a real stream is virtually always locked to,
/// including the `/1001` NTSC-derived fractionals given as exact rationals.
const STANDARD_FPS: [f64; 13] = [
    24000.0 / 1001.0, // 23.976
    24.0,
    25.0,
    30000.0 / 1001.0, // 29.97
    30.0,
    48000.0 / 1001.0, // 47.952
    48.0,
    50.0,
    60000.0 / 1001.0, // 59.94
    60.0,
    100.0,
    120000.0 / 1001.0, // 119.88
    120.0,
];

/// Snap an IVF frame rate to the nearest standard rate when it lands within 1%.
/// The IVF rate is recovered by *averaging* per-frame timestamps, so any sample
/// (bounded or full) drifts a hair off the true rational rate — e.g. 24000/1001
/// measures as 23.977. Nearest-with-tolerance resolves that to 23.976 without
/// forcing a genuinely non-standard rate onto the grid (it's left as measured).
fn snap_to_standard_fps(f: f64) -> f64 {
    STANDARD_FPS
        .into_iter()
        .min_by(|a, b| (a - f).abs().total_cmp(&(b - f).abs()))
        .filter(|s| (s - f).abs() <= f * 0.01)
        .unwrap_or(f)
}

fn find_seq_header(data: &[u8]) -> Option<SeqInfo> {
    obus(data)
        .find(|o| o.obu_type == OBU_SEQUENCE_HEADER)
        .and_then(|o| parse_sequence_header(o.payload))
}

fn build_demux(
    label: &'static str,
    width: u32,
    height: u32,
    fps: Option<f64>,
    frame_count: Option<u64>,
    seq: Option<SeqInfo>,
    chunks: Vec<Chunk>,
) -> Demux {
    // No timestamps or duration box in a raw AV1 stream, so duration is only
    // derivable as frames ÷ frame-rate — both of which we now have for free.
    let duration_secs = match (frame_count, fps) {
        (Some(n), Some(f)) if f > 0.0 => Some(n as f64 / f),
        _ => None,
    };
    let (bit_depth, chroma, color, codec_profile) = match &seq {
        Some(s) => (
            Some(s.bit_depth),
            Some(s.chroma.to_string()),
            s.color.clone(),
            Some(crate::av1::seq::av1_profile_label(s.seq_profile, s.seq_tier, s.seq_level_idx)),
        ),
        None => (None, None, ColorInfo::default(), None),
    };
    Demux {
        container: label,
        codec: Codec::Av1,
        nal_format: NalFormat::LengthPrefixed(0), // unused for AV1 (OBU-walked)
        width,
        height,
        fps,
        duration_secs,
        bit_depth,
        chroma,
        codec_profile,
        stereo: None,
        color,
        dv_config: None,
        // AV1 Dolby Vision (Profile 10) is single-layer, single-track.
        dv_dual_track: false,
        mastering: None,
        content_light: None,
        bitrate: None,
        chunks,
        reassembled: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporal-delimiter OBU: header 0x12 (type 2, `has_size`) + leb128 size 0.
    const TD: [u8; 2] = [0x12, 0x00];

    #[test]
    fn obu_frame_count_is_exact_only_when_walked_whole() {
        // Three temporal units → three access-unit chunks.
        let stream: Vec<u8> = TD.iter().chain(&TD).chain(&TD).copied().collect();

        // Whole-stream walk: the boundary count is the exact frame count.
        let (chunks, count) = split_obu_temporal_units(&stream, true);
        assert_eq!(chunks.len(), 3);
        assert_eq!(count, Some(3));

        // Bounded head walk: a prefix of the frames, so the count (and duration
        // with it) must stay unknown rather than report a wrong total.
        let (chunks, count) = split_obu_temporal_units(&stream, false);
        assert_eq!(chunks.len(), 3, "still indexes what it saw for sampling");
        assert_eq!(count, None);
    }

    #[test]
    fn ivf_fps_snaps_to_nearest_standard_within_tolerance() {
        // Sampling noise around 24000/1001 resolves to the exact NTSC rate...
        assert_eq!(snap_to_standard_fps(23.977), 24000.0 / 1001.0);
        // ...but 24.0 stays 24.0 (nearest is 24, not 23.976)...
        assert_eq!(snap_to_standard_fps(24.001), 24.0);
        // ...and a genuinely non-standard rate is left exactly as measured.
        assert_eq!(snap_to_standard_fps(40.0), 40.0);
    }

    #[test]
    fn no_delimiter_fallback_has_no_frame_count() {
        // A buffer with no temporal delimiter is a single opaque chunk, never a
        // frame count — even under a whole-stream walk.
        let (chunks, count) = split_obu_temporal_units(&[0xAA; 16], true);
        assert_eq!(chunks.len(), 1);
        assert_eq!(count, None);
    }
}
