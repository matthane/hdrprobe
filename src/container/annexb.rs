//! Raw HEVC Annex-B elementary stream. No container metadata — resolution and
//! bit depth come from the SPS; DV profile is inferred from the RPU downstream.

use anyhow::Result;

use crate::container::{Chunk, Codec, Demux, NalFormat};
use crate::hevc::nal::{self, NalRef};
use crate::hevc::sps::parse_sps;
use crate::model::ColorInfo;

/// Bytes NAL-split per sampling window, and the number of windows spread across
/// the file. Below `WINDOW_BYTES * NUM_WINDOWS` a whole-file scan is cheaper than
/// windowing, so it's used unconditionally there and for `--full` at any size.
const WINDOW_BYTES: usize = 2 * 1024 * 1024;
const NUM_WINDOWS: usize = 24;

pub fn demux(data: &[u8], full: bool) -> Result<Demux> {
    let mut nals: Vec<NalRef> = Vec::new();
    if full || data.len() <= WINDOW_BYTES * NUM_WINDOWS {
        nal::split_annexb(data, &mut nals);
    } else {
        split_windows(data, &mut nals);
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
                break;
            }
        }
    }

    let chunks = group_into_aus(&nals);

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
        color,
        dv_config: None,
        // A raw elementary stream is a single track; a Profile-7 EL, if present,
        // is interleaved in it (single track, dual layer).
        dv_dual_track: false,
        mastering: None,
        content_light: None,
        bitrate: None,
        chunks,
        reassembled: None,
    })
}

/// Default-mode scan for large raw streams: NAL-split only a bounded set of byte
/// windows spread across the file (head first) instead of every byte, so cost is
/// O(windows) regardless of file size. Each window starts on a real start code
/// (so no partial leading NAL) and extends to the next start code past its end
/// (so the tail NAL is whole). Windows never overlap. `--full` skips this and
/// scans the whole stream. Static levels live in the head; the spread captures
/// L5 variation — the same head-plus-spread philosophy the TS backend uses.
fn split_windows(data: &[u8], out: &mut Vec<NalRef>) {
    let len = data.len();
    let stride = len / NUM_WINDOWS;
    let mut scanned_to = 0usize;
    let mut tmp: Vec<NalRef> = Vec::new();
    for k in 0..NUM_WINDOWS {
        let ws = match find_start_code(data, (k * stride).max(scanned_to)) {
            Some(s) => s,
            None => break,
        };
        let we = find_start_code(data, (ws + WINDOW_BYTES).min(len)).unwrap_or(len);
        tmp.clear();
        nal::split_annexb(&data[ws..we], &mut tmp);
        for mut nr in tmp.drain(..) {
            nr.start += ws;
            nr.end += ws;
            out.push(nr);
        }
        scanned_to = we;
        if we >= len {
            break;
        }
    }
}

/// First index at/after `from` of a 3-byte (`00 00 01`) Annex-B start-code prefix.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
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
    fn windowed_split_rebases_offsets_and_matches_whole_file_when_it_fits() {
        // With a single covering window, windowed output must equal whole-file
        // split: same types and same *absolute* byte ranges (rebasing correct).
        let d = stream();
        let mut whole = Vec::new();
        nal::split_annexb(&d, &mut whole);
        let mut win = Vec::new();
        split_windows(&d, &mut win);
        assert_eq!(win.len(), whole.len(), "same NAL count");
        for (a, b) in win.iter().zip(&whole) {
            assert_eq!((a.nal_type, a.start, a.end), (b.nal_type, b.start, b.end));
        }
        // Ranges must actually point at the right header bytes in the file.
        for nr in &win {
            assert_eq!(nal_type_of(d[nr.start]), nr.nal_type);
        }
    }

    fn nal_type_of(b: u8) -> u8 {
        (b >> 1) & 0x3F
    }

    #[test]
    fn find_start_code_finds_next_prefix() {
        // Each NAL here is prefixed with a 4-byte code `00 00 00 01`, so the
        // 3-byte `00 00 01` prefix sits one byte in (the leading 00 is padding).
        let d = stream(); // 14-byte NALs
        assert_eq!(find_start_code(&d, 0), Some(1));
        // Next start code after the first NAL's payload: block 1 begins at 14.
        assert_eq!(find_start_code(&d, 4), Some(15));
        assert_eq!(find_start_code(&[1, 2, 3], 0), None);
    }
}
