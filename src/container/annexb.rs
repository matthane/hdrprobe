//! Raw HEVC Annex-B elementary stream. No container metadata — resolution and
//! bit depth come from the SPS; DV profile is inferred from the RPU downstream.

use anyhow::Result;

use crate::container::{Chunk, Codec, Demux, NalFormat, RawFullStream};
use crate::hevc::nal::{self, NalRef};
use crate::hevc::sps::{parse_sps, SpsInfo};
use crate::model::ColorInfo;
use crate::prefetch::Frontier;
use crate::progress::{Phase, Progress};

/// Bytes NAL-split by the default (non-`--full`) scan: a single head window,
/// the same bounded-default shape TS and raw AV1 use. Everything the report
/// needs from a raw stream lives at the head (SPS at byte 0, an RPU/SEI per
/// frame); per-title variation (mid-file L5 aspect changes) is `--full`'s job,
/// and the report's `[sampled]` tags already say so. Must stay `<=`
/// `prefetch::HEAD_WARM` so the generic head warm covers the whole walked span
/// on a network volume (the same coupling `av1::HEAD_SCAN_BYTES` keeps).
pub const HEAD_SCAN_BYTES: usize = 8 << 20; // 8 MiB

pub fn demux(data: &[u8], full: bool, progress: &Progress, frontier: &Frontier) -> Result<Demux> {
    // Always a bounded head split — under `--full` the whole-stream pass
    // belongs to `sample::scan`, which drives `walk_aus` via
    // `Demux::raw_stream` fused with the extraction (one pass over the file,
    // not an index pass plus a scan pass).
    let mut nals: Vec<NalRef> = Vec::new();
    let resume = split_head(data, &mut nals);

    // Resolution / bit depth / colour from the first parsable SPS in the head.
    let mut best: Option<(SpsInfo, u64)> = nals.iter().find_map(|n| {
        if n.nal_type != nal::NAL_SPS {
            return None;
        }
        parse_sps(&data[n.start..n.end]).map(|sps| (sps, n.start as u64))
    });

    // `--full` with no SPS in the head window (a stream cut mid-GOP can put
    // the first IDR — and with it the SPS — past `HEAD_SCAN_BYTES`): keep
    // looking through the stream rather than losing the metadata the old
    // whole-file pass found. Early-exits at the first SPS that parses, so the
    // usual cost is the distance to the first IDR, not the file.
    if full && best.is_none() {
        if let Some(from) = resume {
            best = rescue_sps(data, from, progress, frontier);
        }
    }

    // No container timing box, so frame rate — like colour — comes only from
    // the SPS VUI, when the encoder signalled it.
    let (width, height, bit_depth, chroma, codec_profile, color, fps, sps_offset) = match &best {
        Some((sps, off)) => (
            sps.width,
            sps.height,
            Some(sps.bit_depth),
            Some(sps.chroma_str().to_string()),
            Some(sps.profile_label()),
            sps.color.as_ref().map(crate::container::color_from_vui).unwrap_or_default(),
            sps.frame_rate,
            Some(*off),
        ),
        None => (0, 0, None, None, None, ColorInfo::default(), None, None),
    };

    let chunks = group_into_aus(&nals);
    // The AU carrying that SPS is a RAP — the AU the per-GOP prefix SEIs ride.
    // A stream cut mid-GOP starts with non-RAP AUs, so the sampler must be
    // pointed at this one explicitly (see `Demux::sps_chunk`).
    let sps_chunk = sps_offset
        .and_then(|off| chunks.iter().position(|c| c.offset <= off && off < c.offset + c.size));

    Ok(Demux {
        container: "raw HEVC (Annex-B)",
        codec: Codec::Hevc,
        nal_format: NalFormat::AnnexB,
        width,
        height,
        fps,
        duration_secs: None,
        bit_depth,
        chroma,
        codec_profile,
        stereo: None,
        color,
        dv_config: None,
        // A raw elementary stream is a single track; a Profile-7 EL, if present,
        // is interleaved in it (single track, dual layer).
        dv_dual_track: false,
        mastering: None,
        content_light: None,
        bitrate: None,
        chunks,
        sps_chunk,
        reassembled: None,
        ts_stream: None,
        mkv_stream: None,
        raw_stream: full.then_some(RawFullStream::HevcAnnexB),
    })
}

/// Head scan: NAL-split the head window only, so demux cost is O(window)
/// regardless of file size. The last NAL is cut by the window edge, so it's
/// dropped rather than surfaced as a truncated payload (its AU just ends a NAL
/// early, the same edge TS's head window has); its start offset is returned —
/// a real NAL boundary — so the `--full` SPS rescue can resume there. Under
/// `--full` the whole-stream pass belongs to the sampler's fused walk.
fn split_head(data: &[u8], out: &mut Vec<NalRef>) -> Option<usize> {
    nal::split_annexb(&data[..HEAD_SCAN_BYTES.min(data.len())], out);
    if data.len() > HEAD_SCAN_BYTES {
        return out.pop().map(|n| n.start);
    }
    None
}

/// Byte interval between the SPS rescue's progress/frontier ticks.
const RESCUE_TICK_BYTES: usize = 2 << 20;

/// `--full` fallback when the head window held no SPS: scan forward from the
/// cut NAL's boundary for an SPS that parses, stopping at the first hit.
/// Reported as an `Index` phase — one that legitimately ends short (at the
/// SPS) in the common case, like TS's `sps_rescue`. Metadata only: the fused
/// scan still walks every AU from byte 0, so nothing found here needs pinning.
fn rescue_sps(
    data: &[u8],
    from: usize,
    progress: &Progress,
    frontier: &Frontier,
) -> Option<(SpsInfo, u64)> {
    progress.begin(Phase::Index, data.len() as u64);
    let n = data.len();
    let mut nal_start = Some(from);
    let mut i = from;
    let mut next_tick = from + RESCUE_TICK_BYTES;
    while i + 3 <= n {
        if i >= next_tick {
            frontier.ensure(i as u64);
            progress.update(i as u64);
            next_tick = i + RESCUE_TICK_BYTES;
        }
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = nal_start.take() {
                if let Some(hit) = try_sps(data, s, i) {
                    return Some(hit);
                }
            }
            nal_start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    nal_start.and_then(|s| try_sps(data, s, n))
}

/// Parse `[start, end)` as an SPS if its NAL header says so, trimming the
/// trailing zero a 4-byte start code leaves — the same boundary handling as
/// `nal::split_annexb`.
fn try_sps(data: &[u8], start: usize, mut end: usize) -> Option<(SpsInfo, u64)> {
    while end > start && data[end - 1] == 0 {
        end -= 1;
    }
    if end <= start || (data[start] >> 1) & 0x3F != nal::NAL_SPS {
        return None;
    }
    parse_sps(&data[start..end]).map(|sps| (sps, start as u64))
}

const VCL_MAX: u8 = 31;

/// Push-based access-unit grouper: feed NALs in stream order, and a completed
/// AU chunk pops out when the next AU's first NAL arrives (`finish` flushes
/// the trailing one). A new AU starts at a leading non-VCL NAL or a VCL NAL
/// once the current AU already contains a VCL NAL. Shared by the head index
/// (`group_into_aus`) and the `--full` fused walk (`walk_aus`) so both draw AU
/// boundaries identically.
struct AuGrouper {
    au_start: Option<usize>,
    au_end: usize,
    has_vcl: bool,
}

impl AuGrouper {
    fn new() -> Self {
        AuGrouper { au_start: None, au_end: 0, has_vcl: false }
    }

    fn push(&mut self, n: NalRef) -> Option<Chunk> {
        let is_vcl = n.nal_type <= VCL_MAX;
        let mut done = None;
        if self.has_vcl && (is_vcl || is_au_leader(n.nal_type)) {
            done = self.finish();
        }
        if self.au_start.is_none() {
            self.au_start = Some(n.start);
        }
        self.au_end = n.end;
        if is_vcl {
            self.has_vcl = true;
        }
        done
    }

    fn finish(&mut self) -> Option<Chunk> {
        self.has_vcl = false;
        self.au_start
            .take()
            .map(|start| Chunk { offset: start as u64, size: (self.au_end - start) as u64 })
    }
}

/// Group a flat NAL list into access units so each chunk carries (at most) one
/// RPU alongside its picture NALs.
fn group_into_aus(nals: &[NalRef]) -> Vec<Chunk> {
    let mut g = AuGrouper::new();
    let mut chunks = Vec::new();
    for n in nals {
        if let Some(c) = g.push(*n) {
            chunks.push(c);
        }
    }
    chunks.extend(g.finish());
    chunks
}

/// The `--full` fused walk: split the whole stream and hand each completed
/// access unit to `emit` in file order — the trailing AU at EOF included,
/// matching the old whole-file index. `tick` fires every couple of MiB with
/// the split's byte position (progress + the remote-read frontier); NALs are
/// consumed as found, so nothing file-sized is ever held.
pub fn walk_aus(data: &[u8], tick: impl FnMut(usize), mut emit: impl FnMut(Chunk)) {
    let mut g = AuGrouper::new();
    nal::split_annexb_streamed(
        data,
        |n| {
            if let Some(c) = g.push(n) {
                emit(c);
            }
        },
        tick,
    );
    if let Some(c) = g.finish() {
        emit(c);
    }
}

#[inline]
fn is_au_leader(t: u8) -> bool {
    // Access-unit delimiter, VPS/SPS/PPS, prefix SEI, or DV RPU.
    matches!(t, 32 | 33 | 34 | 35 | 39 | nal::NAL_UNSPEC62_RPU)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a stream of several start-code-delimited NALs.
    fn stream() -> Vec<u8> {
        let mut d = Vec::new();
        for hdr in [0x40u8, 0x42, 0x44, 0x26, 0x02, 0x7C] {
            d.extend_from_slice(&[0, 0, 0, 1, hdr, 0x01]);
            d.extend_from_slice(&[0xAA; 8]);
        }
        d
    }

    #[test]
    fn head_split_matches_whole_file_prefix_and_drops_the_cut_nal() {
        // A stream one NAL past the head window: the bounded split must yield
        // exactly the whole-file split's NALs that end inside the window, and
        // drop the one the edge cuts (never surface a truncated payload).
        let mut d = Vec::new();
        let nal_size = 1 << 20; // 1 MiB per NAL, 4-byte start code + header
        while d.len() <= HEAD_SCAN_BYTES {
            d.extend_from_slice(&[0, 0, 0, 1, 0x42, 0x01]);
            d.resize(d.len() + nal_size, 0xAA);
        }
        let mut whole = Vec::new();
        nal::split_annexb(&d, &mut whole);
        let mut head = Vec::new();
        let resume = split_head(&d, &mut head);
        assert!(head.len() < whole.len(), "the boundary-cut NAL must be dropped");
        for (a, b) in head.iter().zip(&whole) {
            assert_eq!((a.nal_type, a.start, a.end), (b.nal_type, b.start, b.end));
        }
        assert!(head.last().unwrap().end <= HEAD_SCAN_BYTES);
        // The rescue resume point is the cut NAL's own start — a NAL boundary.
        assert_eq!(resume, Some(whole[head.len()].start));

        // At or under the window the demux path takes the whole-file split, and
        // `split_head` itself is a no-op passthrough there too (nothing cut,
        // nothing to resume from).
        let small = stream();
        let mut whole_small = Vec::new();
        nal::split_annexb(&small, &mut whole_small);
        let mut head_small = Vec::new();
        assert_eq!(split_head(&small, &mut head_small), None);
        assert_eq!(head_small.len(), whole_small.len());
    }

    #[test]
    fn full_demux_rescues_an_sps_past_the_head_window() {
        // Real 3840×2160 SPS NAL from a DV Profile 8 stream (structural header
        // bytes only). A stream cut mid-GOP can put the first IDR — and this
        // SPS — past `HEAD_SCAN_BYTES`; the `--full` rescue must recover the
        // metadata the old whole-file pass found, while the bounded default
        // keeps its head-only contract.
        const SPS: [u8; 60] = [
            0x42, 0x01, 0x01, 0x22, 0x20, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x03, 0x00, 0x99, 0xA0, 0x01, 0xE0, 0x20, 0x02, 0x1C, 0x4D, 0x96, 0x66, 0x59,
            0x24, 0x6D, 0x2F, 0x01, 0x6A, 0x12, 0x20, 0x13, 0x6C, 0x20, 0x00, 0x00, 0x7D, 0x20,
            0x00, 0x0B, 0xB8, 0x0C, 0x12, 0x92, 0x0D, 0xC0, 0x00, 0x05, 0xD7, 0x5C, 0x80, 0x00,
            0x05, 0xE6, 0x9E, 0xC4,
        ];
        let mut d = Vec::new();
        while d.len() <= HEAD_SCAN_BYTES {
            d.extend_from_slice(&[0, 0, 0, 1, 0x02, 0x01]); // pre-IDR VCL NAL
            d.resize(d.len() + (1 << 20), 0xAA);
        }
        d.extend_from_slice(&[0, 0, 0, 1]);
        d.extend_from_slice(&SPS);
        d.extend_from_slice(&[0, 0, 0, 1, 0x02, 0x01, 0xAA]);

        let dm = demux(&d, true, &Progress::off(), &Frontier::off()).unwrap();
        assert_eq!((dm.width, dm.height), (3840, 2160), "rescued SPS fills the metadata");
        assert!(matches!(dm.raw_stream, Some(RawFullStream::HevcAnnexB)));

        // Default path: head window only, no rescue, no fused plan.
        let dm = demux(&d, false, &Progress::off(), &Frontier::off()).unwrap();
        assert_eq!(dm.width, 0);
        assert!(dm.raw_stream.is_none());
    }

    #[test]
    fn walk_aus_matches_the_indexed_grouping_including_the_trailing_au() {
        // AUD + VCL, VCL (new AU by VCL-after-VCL), SPS + VCL: three AUs. The
        // fused walk must yield exactly what split + group_into_aus yields,
        // trailing AU included (raw EOF bounds it, unlike TS's open PES).
        let mut d = Vec::new();
        for hdr in [0x46u8, 0x02, 0x02, 0x42, 0x26] {
            d.extend_from_slice(&[0, 0, 0, 1, hdr, 0x01]);
            d.extend_from_slice(&[0xAA; 8]);
        }
        let mut nals = Vec::new();
        nal::split_annexb(&d, &mut nals);
        let indexed = group_into_aus(&nals);
        assert!(indexed.len() >= 2, "test stream must span several AUs");

        let mut walked: Vec<Chunk> = Vec::new();
        walk_aus(&d, |_| {}, |c| walked.push(c));
        assert_eq!(walked.len(), indexed.len());
        for (a, b) in walked.iter().zip(&indexed) {
            assert_eq!((a.offset, a.size), (b.offset, b.size));
        }
    }
}
