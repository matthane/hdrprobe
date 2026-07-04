//! Raw HEVC Annex-B elementary stream. No container metadata — resolution and
//! bit depth come from the SPS; DV profile is inferred from the RPU downstream.

use anyhow::Result;

use crate::container::{Chunk, Codec, Demux, NalFormat};
use crate::hevc::nal::{self, NalRef};
use crate::hevc::sps::parse_sps;
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
    let mut nals: Vec<NalRef> = Vec::new();
    if full {
        // The whole-stream split is `--full`'s single pass over every byte of
        // a possibly huge raw stream — the one walk worth reporting here, and
        // the one the frontier keeps linear on a remote volume (the 2 MiB tick
        // stride sits well inside the frontier's look-ahead).
        progress.begin(Phase::Index, data.len() as u64);
        frontier.ensure(0);
        nal::split_annexb_ticked(data, &mut nals, |pos| {
            frontier.ensure(pos as u64);
            progress.update(pos as u64);
        });
        progress.update(data.len() as u64);
    } else if data.len() <= HEAD_SCAN_BYTES {
        nal::split_annexb(data, &mut nals);
    } else {
        split_head(data, &mut nals);
    }

    // Resolution / bit depth / colour from the first SPS.
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = None;
    let mut chroma = None;
    let mut codec_profile = None;
    let mut color = ColorInfo::default();
    // No container timing box, so frame rate — like colour — comes only from the
    // SPS VUI, when the encoder signalled it.
    let mut fps = None;
    let mut sps_offset = None;
    for n in &nals {
        if n.nal_type == nal::NAL_SPS {
            if let Some(sps) = parse_sps(&data[n.start..n.end]) {
                width = sps.width;
                height = sps.height;
                bit_depth = Some(sps.bit_depth);
                chroma = Some(sps.chroma_str().to_string());
                codec_profile = Some(sps.profile_label());
                if let Some(vui) = &sps.color {
                    color = crate::container::color_from_vui(vui);
                }
                fps = sps.frame_rate;
                sps_offset = Some(n.start as u64);
                break;
            }
        }
    }

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
    })
}

/// Default-mode scan for large raw streams: NAL-split the head window only, so
/// cost is O(window) regardless of file size. The last NAL is cut by the window
/// edge, so it's dropped rather than surfaced as a truncated payload (its AU
/// just ends a NAL early, the same edge TS's head window has). `--full` skips
/// this and scans the whole stream.
fn split_head(data: &[u8], out: &mut Vec<NalRef>) {
    nal::split_annexb(&data[..HEAD_SCAN_BYTES.min(data.len())], out);
    if data.len() > HEAD_SCAN_BYTES {
        out.pop();
    }
}

const VCL_MAX: u8 = 31;

/// Group a flat NAL list into access units so each chunk carries (at most) one
/// RPU alongside its picture NALs. A new AU starts at a leading non-VCL NAL or
/// a VCL NAL once the current AU already contains a VCL NAL.
fn group_into_aus(nals: &[NalRef]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut au_start: Option<usize> = None;
    let mut au_end = 0usize;
    let mut has_vcl = false;

    for n in nals {
        let is_vcl = n.nal_type <= VCL_MAX;
        let boundary = au_start.is_some() && has_vcl && (is_vcl || is_au_leader(n.nal_type));
        if boundary {
            let start = au_start.unwrap();
            chunks.push(Chunk { offset: start as u64, size: (au_end - start) as u64 });
            au_start = None;
            has_vcl = false;
        }
        if au_start.is_none() {
            au_start = Some(n.start);
        }
        au_end = n.end;
        if is_vcl {
            has_vcl = true;
        }
    }
    if let Some(start) = au_start {
        chunks.push(Chunk { offset: start as u64, size: (au_end - start) as u64 });
    }
    chunks
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
        split_head(&d, &mut head);
        assert!(head.len() < whole.len(), "the boundary-cut NAL must be dropped");
        for (a, b) in head.iter().zip(&whole) {
            assert_eq!((a.nal_type, a.start, a.end), (b.nal_type, b.start, b.end));
        }
        assert!(head.last().unwrap().end <= HEAD_SCAN_BYTES);

        // At or under the window the demux path takes the whole-file split, and
        // `split_head` itself is a no-op passthrough there too.
        let small = stream();
        let mut whole_small = Vec::new();
        nal::split_annexb(&small, &mut whole_small);
        let mut head_small = Vec::new();
        split_head(&small, &mut head_small);
        assert_eq!(head_small.len(), whole_small.len());
    }
}
