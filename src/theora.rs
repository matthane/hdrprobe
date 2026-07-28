//! Theora video (Xiph.Org), read from the identification header.
//!
//! Read against the **Theora Specification** (the released bitstream, version
//! 3.2.x): §6.2 for the identification header's field order and widths, §4.3
//! for the `CS` colour space definitions, §1.2 for the bit depth, and §A.2 for
//! the granule position. Cross-checked field by field against
//! `testfiles/sdr/theora.ogv`, whose BOS page declares exactly the 42 bytes the
//! spec fixes.
//!
//! The header is a gap-filler in the [`crate::prores`] / [`crate::vp9`] shape,
//! except that here there is no gap: Ogg records no colour, no dimensions and
//! no frame rate of its own, so this header is the *only* source for every
//! field the report carries about the picture. Four facts are invariants, each
//! pinned by a test naming its source.
//!
//! **The header is big-endian while the Ogg page around it is little-endian.**
//! The spec calls the flip out itself (Vorbis, the other Xiph codec in the same
//! file, is LSb-first again), so one `.ogv` carries up to three conventions and
//! the boundary is exactly the page/payload seam.
//!
//! **`CS` is not CICP.** It is a three-value enum — 0 undefined, 1 Rec. 470M,
//! 2 Rec. 470BG — and the reserved values above it must stay unfilled rather
//! than pass through as codes. Its primaries and matrix map onto CICP code
//! points, and so does its transfer, **by the field's definition rather than by
//! a table row**: Theora overrides both Rec. 470 display gammas with the
//! Rec. 709 opto-electronic function for encoding (§4.3), and H.273 defines
//! `transfer_characteristics` as exactly that encoding-side function of the
//! source picture, so code 1 names Theora's curve verbatim. The Rec. 470
//! *display* gammas the spec also states (2.2 and 2.67) are EOTF-side facts no
//! SDR CICP code has ever carried — BT.601/709 content signals code 1/6 and
//! leaves the display side to BT.1886 the same way — so they are not evidence
//! against the fill. ffprobe reads the field to the same three values,
//! transfer `bt709` for both defined `CS` codes (measured against
//! `testfiles/sdr/theora_cs1.ogv` and `theora_cs2.ogv`; the table is in the
//! format reference), and MediaInfo reports no colour for Theora at all. An
//! earlier revision recorded the transfer as an open question and shipped it
//! unset (plan decision D8); settled 2026-07-26, with sign-off, on the reading
//! above.
//!
//! **The display size is not the coded size.** `FMBW`/`FMBH` count macroblocks,
//! so the coded frame is `FMBW*16 x FMBH*16` and an 854-wide video is coded 864
//! wide; `PICW`/`PICH` are what a viewer sees and are what the report states.
//! They are used only when they sit within 16 pixels of the coded size, which
//! is both the spec's own construction rule and ffmpeg's guard, and otherwise
//! the coded size stands.
//!
//! **The granule position is two fields, not a shift.** `KFGSHIFT` splits it
//! into a keyframe index and an offset from that keyframe, and the frame count
//! is their **sum**; taking `gp >> KFGSHIFT` alone undercounts by up to
//! `2^KFGSHIFT - 1` frames. Streams older than 3.2.1 stored the frame *index*
//! where 3.2.1 and later store the *count*, which is libtheora's own
//! `th_granule_frame` adjustment and is worth one frame at the file's end.

use crate::container::color_from_cicp;
use crate::model::{ColorInfo, ColorSource, ColorSources};

/// The identification header's packet magic: `0x80` (a header packet, type 0)
/// followed by the codec name.
pub const ID_MAGIC: &[u8] = b"\x80theora";

/// Length of the identification header, fixed by the spec. It occupies the
/// logical stream's BOS page alone, so one page is the whole parse.
pub const ID_HEADER_LEN: usize = 42;

/// What the identification header states about the picture.
#[derive(Debug, Clone)]
pub struct IdHeader {
    /// Display width (`PICW`), or the coded width when `PICW` is out of range.
    pub width: u32,
    /// Display height (`PICH`), likewise.
    pub height: u32,
    /// `FRN/FRD`, an exact rational the spec fixes for the whole stream —
    /// Theora is constant-frame-rate by definition, so this never needs a
    /// measured average.
    pub fps: f64,
    /// `PF`, the pixel format: 4:2:0, 4:2:2 or 4:4:4.
    pub chroma: &'static str,
    /// Colour from `CS`, absent when `CS` is 0 (undefined) or a reserved value.
    pub color: Option<(ColorInfo, ColorSources)>,
    /// `PARN`:`PARD`, the pixel aspect ratio, absent when either term is 0
    /// (the spec's "no aspect information" state, and what libtheora writes
    /// by default).
    pub pixel_aspect: Option<(u32, u32)>,
    /// `KFGSHIFT`, the granule position's split point.
    pub kfgshift: u8,
    /// `FRN`, kept alongside `fps` because the duration is computed as an exact
    /// rational (`frames * FRD / FRN`) rather than through the rounded rate.
    pub frn: u32,
    /// `FRD`.
    pub frd: u32,
    /// True for a stream older than 3.2.1, whose granule position holds the
    /// frame index rather than the frame count.
    pub pre_321: bool,
}

/// Theora's bit depth. Spec §1.2 states the format "does not support
/// bit-depths larger than 8 bits per component" and that wider support "is not
/// planned", so this is a format constant in the same sense as MPEG-2's — not
/// a field that was read.
pub const BIT_DEPTH: u8 = 8;

/// Parse the identification header from a BOS page's payload.
///
/// `None` for anything the spec or libtheora's own header validation rejects:
/// a wrong magic, a major version other than 3, a zero macroblock dimension, a
/// zero frame-rate term, the reserved pixel format, or a non-zero value in the
/// trailing reserved bits. Those checks are what stop unrelated bytes that
/// happen to open with the magic from producing a plausible-looking report.
pub fn parse_id_header(p: &[u8]) -> Option<IdHeader> {
    if p.len() < ID_HEADER_LEN || !p.starts_with(ID_MAGIC) {
        return None;
    }
    let (vmaj, vmin, vrev) = (p[7], p[8], p[9]);
    // The 42-byte layout below is defined for **3.2.x exactly**, so both ends
    // of that are refused: the pre-3.2 alphas laid the header out differently,
    // and a future 3.3 would be read with fields this build cannot know are
    // still there. An earlier form of this gate said `vmin < 2`, which refused
    // the old streams while admitting the future ones — the opposite of what
    // its own reasoning asked for.
    if vmaj != 3 || vmin != 2 {
        return None;
    }

    let fmbw = be16(p, 10) as u32;
    let fmbh = be16(p, 12) as u32;
    if fmbw == 0 || fmbh == 0 {
        return None;
    }
    // 20 of the 24 bits read are meaningful, which is why the coded frame can
    // never overflow: `FMBW` is 16 bits of macroblocks, so `FMBW*16` fits a u32
    // with room to spare.
    let picw = be24(p, 14);
    let pich = be24(p, 17);
    let (picx, picy) = (p[20] as u32, p[21] as u32);
    let (coded_w, coded_h) = (fmbw * 16, fmbh * 16);

    let frn = be32(p, 22);
    let frd = be32(p, 26);
    if frn == 0 || frd == 0 {
        return None;
    }
    // Both terms are unvalidated 32-bit integers, so their quotient needs the
    // same plausibility bound every other declared rate in the tree gets: a
    // `FRN` of `0xFFFFFFFF` otherwise reports four billion frames per second
    // and, through the duration it divides, a bitrate of **36.8 Tb/s from the
    // 53 KiB corpus file** (measured) — while `FRD` of `0xFFFFFFFF` prints
    // `0.000 fps`.
    let fps = crate::container::plausible_fps(frn as f64 / frd as f64)?;

    // `PF` 1 is reserved and the spec says such a stream is undecodable, so it
    // is refused rather than reported with a guessed chroma.
    let chroma = match (p[41] >> 3) & 0x03 {
        0 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => return None,
    };
    // The trailing three bits are reserved and must be zero; libtheora rejects
    // a header that sets them, and so does this, because a non-zero value means
    // the bytes are not the layout above.
    if p[41] & 0x07 != 0 {
        return None;
    }

    // The picture region must fit inside the coded frame (the spec's own
    // constraint) and, since `FMBW` is `ceil(PICW/16)`, must also sit within 16
    // pixels of it. ffmpeg applies exactly that pair and keeps the coded size
    // when either fails; a display size that fails them is not describing this
    // frame, and the coded size always is.
    let usable = |pic: u32, off: u32, coded: u32| {
        pic > 0 && pic + off <= coded && pic + 16 > coded
    };
    let width = if usable(picw, picx, coded_w) { picw } else { coded_w };
    let height = if usable(pich, picy, coded_h) { pich } else { coded_h };

    let (parn, pard) = (be24(p, 30), be24(p, 33));
    Some(IdHeader {
        width,
        height,
        fps,
        chroma,
        pixel_aspect: (parn > 0 && pard > 0).then_some((parn, pard)),
        color: color_from_cs(p[36]),
        kfgshift: ((p[40] & 0x03) << 3) | (p[41] >> 5),
        frn,
        frd,
        // The granule position stored the frame index before 3.2.1 and the
        // count from 3.2.1 on — libtheora's `th_granule_frame` subtracts
        // exactly this version check.
        pre_321: (vmaj, vmin, vrev) < (3, 2, 1),
    })
}

/// Translate `CS` into the CICP code points its chromaticities, transfer
/// function and matrix coefficients match.
///
/// The primaries map cleanly — `CS` 1 is Rec. 470M's R/G/B and Illuminant C,
/// `CS` 2 is Rec. 470BG's and D65 — and both colour spaces state the same
/// Kr/Kb (0.299/0.114), which CICP 5 and CICP 6 share exactly. Since the two
/// matrix codes are numerically identical and differ only in which 601 system
/// they name, each `CS` takes the one whose system its primaries already named.
/// The transfer is CICP 1 for both: H.273's `transfer_characteristics` is the
/// source's opto-electronic function, and Theora §4.3 fixes that at Rec. 709's
/// curve for both defined colour spaces (their differing *display* gammas are
/// EOTF-side facts the code point does not carry — see the module doc).
/// The range is limited for both: the spec fixes offset 16 and excursion 219
/// inside each colour space definition, so it is known exactly where `CS` is
/// and unknown where it is not.
fn color_from_cs(cs: u8) -> Option<(ColorInfo, ColorSources)> {
    let (primaries, matrix) = match cs {
        1 => (4, 6), // Rec. 470M primaries, the 525-line matrix
        2 => (5, 5), // Rec. 470BG primaries, the 625-line matrix
        _ => return None,
    };
    // Routing through the shared decoder keeps these labels the same objects
    // the container-signalled ones are.
    Some(color_from_cicp(primaries, 1, matrix, Some(false), ColorSource::Stream))
}

/// Frames finished by a granule position, or `None` for the "no packet
/// finishes here" marker (`-1`) and for a value the split cannot describe.
///
/// The count is the keyframe index plus the offset from it, both halves of the
/// `KFGSHIFT` split. Pre-3.2.1 streams stored the frame index, one less than
/// the count, so those gain the missing frame back.
pub fn granule_frames(gp: i64, kfgshift: u8, pre_321: bool) -> Option<u64> {
    if gp < 0 || kfgshift > 63 {
        return None;
    }
    let gp = gp as u64;
    let keyframe = gp >> kfgshift;
    let offset = gp & ((1u64 << kfgshift) - 1);
    let frames = keyframe.checked_add(offset)?;
    if pre_321 {
        frames.checked_add(1)
    } else {
        Some(frames)
    }
}

fn be16(p: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([p[at], p[at + 1]])
}
fn be24(p: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([0, p[at], p[at + 1], p[at + 2]])
}
fn be32(p: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `testfiles/sdr/theora.ogv` bytes 0x1C..0x46 verbatim: the whole
    /// identification header, alone in its BOS page as the spec requires.
    /// 320x240, 25/1 fps, 4:2:0, `KFGSHIFT` 6, `CS` 0 (ffmpeg's libtheora
    /// wrapper never sets it).
    const REAL_ID: [u8; ID_HEADER_LEN] = [
        0x80, 0x74, 0x68, 0x65, 0x6f, 0x72, 0x61, 0x03, 0x02, 0x01, 0x00, 0x14, 0x00, 0x0f, 0x00,
        0x01, 0x40, 0x00, 0x00, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x19, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x03, 0x0d, 0x40, 0x00, 0xc0,
    ];

    #[test]
    fn real_identification_header_decodes_every_field() {
        let h = parse_id_header(&REAL_ID).expect("valid header");
        assert_eq!((h.width, h.height), (320, 240));
        assert_eq!((h.frn, h.frd), (25, 1));
        assert_eq!(h.fps, 25.0);
        assert_eq!(h.chroma, "4:2:0");
        assert_eq!(h.kfgshift, 6);
        assert!(!h.pre_321, "VMAJ.VMIN.VREV is 3.2.1");
        // `CS` is 0 here, so nothing about colour is signalled and nothing is
        // filled — the D9 case, and what every reference tool reports for this
        // file.
        assert!(h.color.is_none());
    }

    #[test]
    fn the_header_is_read_big_endian() {
        // The Ogg page around it is little-endian; reading these two 16-bit
        // macroblock counts that way gives 5120x3840 rather than 320x240, which
        // is the whole reason the boundary is called out.
        let h = parse_id_header(&REAL_ID).expect("valid header");
        assert_eq!((h.width, h.height), (320, 240));
        assert_ne!((h.width, h.height), (5120, 3840));
    }

    /// Patch one byte of the real header, so every test below differs from a
    /// known-good file in exactly the field it names.
    fn patched(at: usize, to: u8) -> [u8; ID_HEADER_LEN] {
        let mut h = REAL_ID;
        h[at] = to;
        h
    }

    #[test]
    fn cs_1_and_2_map_to_their_cicp_code_points_with_bt709_transfer() {
        let (c1, s1) = parse_id_header(&patched(36, 1)).unwrap().color.expect("CS 1 maps");
        assert_eq!(c1.primaries.as_deref(), Some("BT.470M"));
        assert_eq!(c1.matrix.as_deref(), Some("BT.601 (NTSC)"));
        assert_eq!(c1.range.as_deref(), Some("limited"));
        // The load-bearing half: Theora fixes the encoding-side function at
        // Rec. 709's curve for both colour spaces, which is what H.273's
        // `transfer_characteristics` describes, so CICP 1 names it verbatim
        // (ffprobe answers `bt709` for both; the display gammas are EOTF-side
        // and not the code point's business). Plan decision D8, reversed with
        // sign-off 2026-07-26.
        assert_eq!(c1.transfer.as_deref(), Some("BT.709"));
        assert_eq!(s1.transfer, Some(ColorSource::Stream));
        assert_eq!(s1.primaries, Some(ColorSource::Stream));
        assert_eq!(s1.matrix, Some(ColorSource::Stream));

        let (c2, _) = parse_id_header(&patched(36, 2)).unwrap().color.expect("CS 2 maps");
        assert_eq!(c2.primaries.as_deref(), Some("BT.601 (PAL)"));
        assert_eq!(c2.matrix.as_deref(), Some("BT.601 (PAL)"));
        assert_eq!(c2.transfer.as_deref(), Some("BT.709"));
    }

    #[test]
    fn reserved_cs_values_fill_nothing_rather_than_passing_through_as_cicp() {
        // `CS` 3..255 are reserved. Treated as CICP they would decode: 3 names
        // no primaries but 5 would say "BT.601 (PAL)" and 9 "BT.2020",
        // inventing a wide-gamut verdict from a value the spec has not
        // assigned. Every one of them must fill nothing at all.
        for cs in [3u8, 4, 5, 9, 16, 255] {
            let h = parse_id_header(&patched(36, cs)).expect("header still valid");
            assert!(h.color.is_none(), "CS {cs} is reserved and must fill nothing");
        }
    }

    #[test]
    fn pixel_format_1_is_reserved_and_refuses_the_stream() {
        // byte 41 packs KFGSHIFT's low bits, PF, and three reserved bits.
        // 0xc0 is PF 0 with KFGSHIFT low bits 6; setting PF to 1 gives 0xc8.
        assert!(parse_id_header(&patched(41, 0xc8)).is_none(), "PF 1 is undecodable");
        // PF 2 and 3 are the other two real formats.
        assert_eq!(parse_id_header(&patched(41, 0xd0)).unwrap().chroma, "4:2:2");
        assert_eq!(parse_id_header(&patched(41, 0xd8)).unwrap().chroma, "4:4:4");
    }

    #[test]
    fn the_reserved_trailing_bits_must_be_zero() {
        // libtheora rejects a header setting them; a non-zero value means the
        // bytes being read are not this layout.
        assert!(parse_id_header(&patched(41, 0xc1)).is_none());
        assert!(parse_id_header(&patched(41, 0xc7)).is_none());
    }

    #[test]
    fn structural_zeros_and_wrong_versions_are_refused() {
        assert!(parse_id_header(&[]).is_none());
        assert!(parse_id_header(&REAL_ID[..41]).is_none(), "one byte short");
        assert!(parse_id_header(&patched(0, 0x81)).is_none(), "wrong packet type");
        assert!(parse_id_header(&patched(1, b'T')).is_none(), "wrong magic");
        // The 42-byte layout is 3.2.x's, so both ends are refused. An earlier
        // gate tested `vmin < 2`, which refused the pre-3.2 alphas but admitted
        // a hypothetical 3.3 — the opposite of what its own reasoning asked.
        assert!(parse_id_header(&patched(7, 2)).is_none(), "major version 2");
        assert!(parse_id_header(&patched(7, 4)).is_none(), "major version 4");
        assert!(parse_id_header(&patched(8, 1)).is_none(), "minor version 1");
        assert!(parse_id_header(&patched(8, 3)).is_none(), "minor version 3 is not this layout");
        assert!(parse_id_header(&patched(9, 0)).is_some(), "3.2.0 is this layout");
        // FMBW and FMBH are the macroblock counts every other dimension is
        // derived from; zero makes the coded frame zero-sized.
        assert!(parse_id_header(&patched(11, 0)).is_none(), "FMBW 0");
        assert!(parse_id_header(&patched(13, 0)).is_none(), "FMBH 0");
        // A zero frame-rate term would divide by zero or report 0 fps.
        assert!(parse_id_header(&patched(25, 0)).is_none(), "FRN 0");
        assert!(parse_id_header(&patched(29, 0)).is_none(), "FRD 0");
    }

    #[test]
    fn an_implausible_frame_rate_is_refused_rather_than_divided() {
        // `FRN` and `FRD` are unvalidated 32-bit integers. Rejecting only zero
        // let 0xFFFFFFFF through as four billion frames per second, and the
        // 1.16e-8 second duration that implies produced a 36.8 Tb/s bitrate
        // from a 53 KiB file; the reciprocal case printed `0.000 fps`.
        let mut fast = REAL_ID;
        fast[22..26].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse_id_header(&fast).is_none(), "4 billion fps is a misread field");

        let mut slow = REAL_ID;
        slow[26..30].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse_id_header(&slow).is_none(), "0.000 fps reads as a stated zero");

        // A real low rate is still accepted: 1 fps timelapse, and 24000/1001.
        let mut one = REAL_ID;
        one[22..26].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(parse_id_header(&one).map(|h| h.fps), Some(1.0));
        let mut ntsc = REAL_ID;
        ntsc[22..26].copy_from_slice(&24_000u32.to_be_bytes());
        ntsc[26..30].copy_from_slice(&1001u32.to_be_bytes());
        let h = parse_id_header(&ntsc).expect("23.976 is ordinary");
        assert_eq!((h.frn, h.frd), (24_000, 1001));
    }

    #[test]
    fn a_display_size_outside_the_coded_frame_falls_back_to_the_coded_size() {
        // FMBW 20 makes the coded frame 320 wide, so PICW must be 305..=320.
        // 0x0140 is 320; 0x0100 is 256, more than 16 short, so it cannot be
        // this frame's display width and the coded 320 stands (ffmpeg's guard).
        let mut h = REAL_ID;
        h[15] = 0x01;
        h[16] = 0x00;
        assert_eq!(parse_id_header(&h).unwrap().width, 320);

        // 0x013a is 314, inside the window, and is used verbatim — this is the
        // ordinary case for a video whose width is not a multiple of 16.
        h[15] = 0x01;
        h[16] = 0x3a;
        assert_eq!(parse_id_header(&h).unwrap().width, 314);

        // A PICW larger than the coded frame is impossible; the coded size wins.
        h[15] = 0x02;
        h[16] = 0x00;
        assert_eq!(parse_id_header(&h).unwrap().width, 320);

        // PICX pushes the region past the right edge: 314 + 8 > 320.
        h[15] = 0x01;
        h[16] = 0x3a;
        h[20] = 8;
        assert_eq!(parse_id_header(&h).unwrap().width, 320);
    }

    #[test]
    fn granule_position_sums_both_halves_of_the_split() {
        // `testfiles/sdr/theora.ogv`'s EOS page: granule 3137, KFGSHIFT 6.
        // 3137 = (49 << 6) | 1, so the keyframe index is 49 and the offset 1 —
        // 50 frames, which at 25 fps is the 2.000 s ffprobe reports.
        assert_eq!(granule_frames(3137, 6, false), Some(50));
        // Taking only the shift is the documented mistake: it undercounts by
        // the offset, here by one frame and in general by up to 2^shift - 1.
        assert_ne!(granule_frames(3137, 6, false), Some(3137 >> 6));

        // A pre-3.2.1 stream stored the index, so the count is one more.
        assert_eq!(granule_frames(3137, 6, true), Some(51));

        // -1 marks a page on which no packet finishes; it is not a frame count.
        assert_eq!(granule_frames(-1, 6, false), None);
        // A shift of 0 puts everything in the keyframe half.
        assert_eq!(granule_frames(42, 0, false), Some(42));
        // The field is 5 bits wide, so it can never exceed 31, but the guard
        // keeps the shift below the width of the type it is applied to.
        assert_eq!(granule_frames(1, 64, false), None);
        // The largest legal granule cannot overflow the sum.
        assert_eq!(granule_frames(i64::MAX, 31, false), Some((i64::MAX as u64 >> 31) + 0x7FFF_FFFF));
    }
}
