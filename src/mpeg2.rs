//! MPEG-1 (ISO/IEC 11172-2) and MPEG-2 (ITU-T H.262 | ISO/IEC 13818-2) video
//! sequence parsing.
//!
//! Everything the report needs rides the `sequence_header` and the two sequence
//! extensions that follow it, all at the head of the stream, so this is a
//! head-only parse in the same shape as [`crate::prores`] and [`crate::vp9`]:
//! container signalling keeps authority and the bitstream only fills gaps. For
//! these codecs that matters more than for ProRes, because the bitstream is
//! usually the *only* colour source. MKV and MP4 carry no colour element for an
//! MPEG-2 track unless a muxer added one, and TS and raw elementary streams
//! have nowhere to put one at all.
//!
//! Two fields are deliberately absent rather than derived.
//!
//! **Bit depth is not signalled**, and every profile MPEG-1 or MPEG-2 defines
//! codes 8-bit, 4:2:2 and High included, so 8 is a spec constant here in the
//! same sense as ProRes reporting its profile family's depth.
//! `picture_coding_extension`'s `intra_dc_precision` is *not* a depth field: it
//! takes values 8..11 and scales DC inverse quantisation, so reporting it would
//! put "11-bit" on an 8-bit stream.
//!
//! **Colour has no defaults.** H.262 says an absent `sequence_display_extension`
//! leaves colour "implicitly defined by the application", so nothing is filled
//! and the fields stay genuinely unsignalled. Filling BT.601 and tagging it as
//! signalled would be a fabrication, and the common case: ffmpeg's mpeg2video
//! encoder writes no display extension at all unless asked.

use crate::model::{ColorInfo, ColorSource, ColorSources};

/// `sequence_header_code`.
const SEQUENCE_HEADER: u8 = 0xB3;
/// `extension_start_code`.
const EXTENSION: u8 = 0xB5;
/// `user_data_start_code`.
const USER_DATA: u8 = 0xB2;
/// `extension_start_code_identifier` values (Table 6-2) this module reads.
const EXT_SEQUENCE: u8 = 1;
const EXT_SEQUENCE_DISPLAY: u8 = 2;

/// What an MPEG-1/2 video sequence declares about itself.
pub struct SeqInfo {
    pub width: u32,
    pub height: u32,
    /// `frame_rate_code` through Table 6-4, scaled by the sequence extension's
    /// extension fields. `None` for the forbidden and reserved codes.
    pub fps: Option<f64>,
    /// `chroma_format` (Table 6-5). MPEG-1 has no sequence extension and is
    /// always 4:2:0.
    pub chroma: Option<&'static str>,
    /// Always 8. See the module docs: not signalled, and a spec constant.
    pub bit_depth: u8,
    /// `profile_and_level_indication` as `Profile@Level`, `None` for MPEG-1
    /// (which has no such field) and for reserved codes.
    pub profile_level: Option<String>,
    /// The `sequence_display_extension`'s CICP triple, empty when the extension
    /// is absent or its `colour_description` flag is clear. Never carries
    /// `range`: MPEG-2 does not signal one (limited is normative, which is not
    /// the same as signalled), so the container keeps that field.
    pub color: (ColorInfo, ColorSources),
    /// Aspect from `aspect_ratio_information`, whose meaning flips with the
    /// codec (H.262 Table 6-3 vs the 11172-2 `pel_aspect_ratio` table): MPEG-2
    /// signals a **display** ratio (code 1 aside), MPEG-1 a **pixel** ratio.
    /// Exactly one of the two is `Some` for a defined code; both stay `None`
    /// for the reserved ones.
    pub pixel_aspect: Option<(u32, u32)>,
    pub display_aspect: Option<(u32, u32)>,
    /// `"progressive"` when MPEG-2's `progressive_sequence` is set, or for
    /// MPEG-1 structurally (11172-2 has no interlace anywhere — the same
    /// class as its 4:2:0). A clear flag fills nothing: it means "may contain
    /// interlaced pictures", and a film-sourced DVD codes progressive frames
    /// under a clear flag — both reference tools read the pictures and say
    /// Progressive there, so an "interlaced" from the flag alone would
    /// disagree with both on the most common DVD case.
    pub scan_type: Option<&'static str>,
    /// True once a `sequence_extension` has been read. Its presence is exactly
    /// what separates 13818-2 from 11172-2, so a raw stream's codec identity
    /// comes from this rather than from any container claim.
    pub is_mpeg2: bool,
}

/// Parse the first video sequence in `data`.
///
/// Scans for the `sequence_header`, rather than assuming byte 0, because a
/// reassembled transport or program stream can begin mid-GOP. Returns `None`
/// when no plausible sequence header is found, which is also what makes this
/// safe to run over bytes that may not be MPEG at all.
pub fn parse_sequence(data: &[u8]) -> Option<SeqInfo> {
    let sh = find_sequence_header(data)?;
    // Bytes 4..11 of the header, counting the 4-byte start code as 0..3.
    let h = sh + 4;
    let mut info = SeqInfo {
        width: horizontal_size_value(data, h),
        height: vertical_size_value(data, h),
        fps: frame_rate(data[h + 3] & 0x0F),
        // Filled at the end: 4:2:0 is *MPEG-1's* constant, and stating it up
        // front would leave it standing on an MPEG-2 stream whose sequence
        // extension was truncated, printing a chroma format nothing read.
        chroma: None,
        bit_depth: 8,
        profile_level: None,
        color: (ColorInfo::default(), ColorSources::default()),
        pixel_aspect: None,
        display_aspect: None,
        scan_type: None,
        is_mpeg2: false,
    };
    let aspect_code = data[h + 3] >> 4;

    // The quantiser matrices that may follow the fixed header are bit-packed
    // and never needed, so the end of the header is the next start code. MPEG
    // has no emulation prevention, but it does not need any: matrix
    // coefficients are non-zero, marker bits break long zero runs, and user
    // data may not contain 23 consecutive zero bits, so a `00 00 01` in the
    // byte stream is always a real start code.
    let mut pos = next_start_code(data, h + 8)?;
    loop {
        // Where this element's payload ends: the next start code, or the buffer.
        // Extension parsing is bounded by it so a truncated extension cannot
        // read the following start code's bytes as its own fields (a 4-byte
        // sequence extension followed by a GOP header decoded as `4:2:2@High`).
        let next = next_start_code(data, pos + 4);
        let elem_end = next.unwrap_or(data.len());
        match data[pos + 3] {
            EXTENSION => {
                let e = pos + 4;
                match data.get(e).map(|b| b >> 4) {
                    Some(EXT_SEQUENCE) => {
                        if let Some(seq) = parse_sequence_extension(data, e, elem_end) {
                            // 13818-2 by construction: 11172-2 defines no
                            // extensions at all, so a *complete* one settles the
                            // codec. Set only on success, or a truncated
                            // extension would promote the stream to MPEG-2 while
                            // leaving every field it should have filled unread.
                            info.is_mpeg2 = true;
                            info.width |= seq.horizontal_size_extension << 12;
                            info.height |= seq.vertical_size_extension << 12;
                            info.chroma = chroma_format(seq.chroma_format);
                            info.profile_level = profile_level_label(seq.profile_and_level);
                            // Every defined profile constrains both extension
                            // fields to 0, so this is normally a no-op, but it
                            // is a spec field and costs one multiply.
                            info.fps = info.fps.map(|f| {
                                f * (seq.frame_rate_extension_n as f64 + 1.0)
                                    / (seq.frame_rate_extension_d as f64 + 1.0)
                            });
                            // Affirmative only: `progressive_sequence` set is
                            // a declaration, but clear means "may contain
                            // interlaced pictures" (H.262 §6.3.5) — a
                            // film-sourced DVD codes progressive frames under
                            // a clear flag, and both reference tools read the
                            // *pictures* and say Progressive there. Reporting
                            // "interlaced" from the clear flag would disagree
                            // with both on the most common DVD case, so the
                            // clear flag fills nothing.
                            info.scan_type = seq.progressive.then_some("progressive");
                        }
                    }
                    // Only after a sequence extension has been seen. 11172-2
                    // defines no extensions, so reading a display extension
                    // without one would report an MPEG-2-only structure's colour
                    // on a stream the same parse calls MPEG-1.
                    Some(EXT_SEQUENCE_DISPLAY) if info.is_mpeg2 => {
                        if let Some(c) = parse_display_extension(data, e, elem_end) {
                            info.color = c;
                        }
                    }
                    _ => {}
                }
            }
            // User data sits between the extensions and carries nothing we read.
            USER_DATA => {}
            // A GOP header, a picture, or anything else: the sequence-level
            // data is over.
            _ => break,
        }
        pos = match next {
            Some(p) => p,
            None => break,
        };
    }
    // MPEG-1 has no sequence extension and therefore no `chroma_format` field:
    // 11172-2 codes 4:2:0 and only 4:2:0, which makes this a spec constant in
    // the same sense as the bit depth, not a guess. An MPEG-2 stream took its
    // value from the extension above, or left it unknown if that was truncated.
    if !info.is_mpeg2 {
        info.chroma = Some("4:2:0");
        // Frame-based by construction: 11172-2 defines no interlace anywhere,
        // making this the structural constant the module doc groups with the
        // bit depth and the 4:2:0.
        info.scan_type = Some("progressive");
    }
    // `aspect_ratio_information` reads per codec, which is only known now.
    // H.262 Table 6-3: 1 is square *pixels*, 2..4 are **display** ratios, the
    // rest reserved and filled with nothing. The 11172-2 `pel_aspect_ratio`
    // table is a **pixel** height/width (so the pixel ratio is its inverse),
    // with the disputed indices 8 and 12 resolved to 0.9375/1.1250 — the
    // geometry and interpolation case is in the format reference §1, against
    // ffmpeg's draft-era 0.9157/1.0950.
    if info.is_mpeg2 {
        match aspect_code {
            1 => info.pixel_aspect = Some((1, 1)),
            2 => info.display_aspect = Some((4, 3)),
            3 => info.display_aspect = Some((16, 9)),
            4 => info.display_aspect = Some((221, 100)),
            _ => {}
        }
    } else if let Some(&pel) = MPEG1_PEL_ASPECT.get(aspect_code as usize) {
        if pel > 0 {
            info.pixel_aspect = Some((10_000, pel));
        }
    }
    Some(info)
}

/// 11172-2 `pel_aspect_ratio` × 10⁴, indexed by `aspect_ratio_information`
/// (0 and 15 are forbidden/reserved and the leading 0 entry covers the first).
/// The value is pel height/width, so the pixel ratio reported is its inverse.
const MPEG1_PEL_ASPECT: [u32; 15] = [
    0, 10_000, 6_735, 7_031, 7_615, 8_055, 8_437, 8_935, 9_375, 9_815, 10_255, 10_695, 11_250,
    11_575, 12_015,
];

/// Offset of the first plausible `sequence_header` in `data`, as the index of
/// its start code's first zero byte.
///
/// The plausibility checks matter because a caller may be resyncing blind into
/// a stream that is not MPEG at all: `frame_rate_code` must be one of the eight
/// defined values, `aspect_ratio_information` must not be the forbidden 0 or
/// the reserved 15, and the marker bit in the middle of the VBV field must be
/// set, and neither 12-bit size field may be zero.
///
/// The aspect range admits both readings of that field, whose value spaces
/// differ: MPEG-2 defines 1..=4 and MPEG-1 1..=14, and both forbid 0 and
/// reserve 15. The size check costs one theoretical case, a stream whose size
/// lives entirely in the sequence extension's two high bits, which would mean a
/// picture at least 4096 wide or tall; no defined level exceeds 1920x1152, so
/// the guard is worth far more than the case it gives up.
fn find_sequence_header(data: &[u8]) -> Option<usize> {
    let mut pos = next_start_code(data, 0)?;
    loop {
        if data[pos + 3] == SEQUENCE_HEADER {
            let h = pos + 4;
            // Eight fixed bytes must be readable before anything is decoded.
            if h + 8 <= data.len() {
                let aspect = data[h + 3] >> 4;
                let rate = data[h + 3] & 0x0F;
                let marker = data[h + 6] & 0x20 != 0;
                if (1..=14).contains(&aspect)
                    && (1..=8).contains(&rate)
                    && marker
                    && horizontal_size_value(data, h) != 0
                    && vertical_size_value(data, h) != 0
                {
                    return Some(pos);
                }
            }
        }
        pos = next_start_code(data, pos + 3)?;
    }
}

/// `horizontal_size_value`: byte 4 plus the high nibble of byte 5.
fn horizontal_size_value(data: &[u8], h: usize) -> u32 {
    (data[h] as u32) << 4 | (data[h + 1] as u32) >> 4
}

/// `vertical_size_value`: the low nibble of byte 5 plus byte 6.
fn vertical_size_value(data: &[u8], h: usize) -> u32 {
    ((data[h + 1] as u32) & 0x0F) << 8 | data[h + 2] as u32
}

/// Index of the next `00 00 01` prefix at or after `from`, positioned so the
/// start-code value byte at `+3` is always readable.
pub(crate) fn next_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The `sequence_extension` fields this module reads, `e` being its
/// `extension_start_code_identifier` byte.
struct SequenceExtension {
    profile_and_level: u8,
    progressive: bool,
    chroma_format: u8,
    horizontal_size_extension: u32,
    vertical_size_extension: u32,
    frame_rate_extension_n: u8,
    frame_rate_extension_d: u8,
}

fn parse_sequence_extension(data: &[u8], e: usize, elem_end: usize) -> Option<SequenceExtension> {
    // Six bytes: ext_id(4) pli(8) progressive(1) chroma(2) h_ext(2) v_ext(2)
    // bit_rate_ext(12) marker(1) vbv_ext(8) low_delay(1) fr_ext_n(2) fr_ext_d(5).
    // Bounded by the element's own end, not just the buffer's: a short
    // extension must fail rather than read the next start code as its fields.
    if e + 6 > elem_end {
        return None;
    }
    let b = data.get(e..e + 6)?;
    Some(SequenceExtension {
        profile_and_level: (b[0] & 0x0F) << 4 | b[1] >> 4,
        // Coded height rounds to 32 lines when this is clear, which is where
        // 1080 becomes 1088, but the reported size is the header's own value.
        progressive: (b[1] >> 3) & 0x01 != 0,
        chroma_format: (b[1] >> 1) & 0x03,
        horizontal_size_extension: ((b[1] & 0x01) << 1 | b[2] >> 7) as u32,
        vertical_size_extension: ((b[2] >> 5) & 0x03) as u32,
        frame_rate_extension_n: (b[5] >> 5) & 0x03,
        frame_rate_extension_d: b[5] & 0x1F,
    })
}

/// The `sequence_display_extension`'s colour description, `e` being its
/// `extension_start_code_identifier` byte. `None` when the extension carries no
/// colour description, which is the ordinary case: the three CICP bytes are
/// present only when `colour_description` is set.
fn parse_display_extension(
    data: &[u8],
    e: usize,
    elem_end: usize,
) -> Option<(ColorInfo, ColorSources)> {
    // Bounded by the element's end for the same reason as the sequence
    // extension: a truncated one must decline rather than read past itself.
    if e + 4 > elem_end {
        return None;
    }
    let b = data.get(e..e + 4)?;
    if b[0] & 0x01 == 0 {
        return None;
    }
    // `display_horizontal_size`/`_vertical_size` follow these three bytes. They
    // describe the intended display rectangle rather than the coded picture,
    // and the report has no field for them.
    //
    // **Value 0 is Forbidden here, not CICP 0.** H.262 Tables 6-7/6-8/6-9 mark
    // it forbidden where CICP defines Identity/GBR and RGB, so a stray 0 from a
    // broken encoder must read as unsignalled rather than being decoded: the
    // CICP table would otherwise print `"RGB"` as this stream's matrix, and on
    // a file whose primaries and transfer are also 0 that becomes the entire
    // Color line. ffmpeg patches a read 0 to 2 and warns; mapping it to 2, the
    // explicit "unspecified" code, is the same answer in this codebase's terms.
    let cicp = |v: u8| if v == 0 { 2u16 } else { v as u16 };
    Some(crate::container::color_from_cicp(
        cicp(b[1]),
        cicp(b[2]),
        cicp(b[3]),
        // Not a signal: MPEG-2 has no range field at all. Limited range is
        // normative, but normative is not the same as signalled, and a
        // container-supplied range must keep its own value and provenance.
        None,
        ColorSource::Stream,
    ))
}

/// `frame_rate_code`, Table 6-4.
///
/// Codes 9..=15 are reserved and 0 is forbidden, so all seven yield `None`.
/// ffmpeg's `ff_mpeg12_frame_rate_tab` carries non-standard Xing and libmpeg3
/// entries at 9..=13; mirroring it would invent a frame rate.
fn frame_rate(code: u8) -> Option<f64> {
    Some(match code {
        1 => 24000.0 / 1001.0,
        2 => 24.0,
        3 => 25.0,
        4 => 30000.0 / 1001.0,
        5 => 30.0,
        6 => 50.0,
        7 => 60000.0 / 1001.0,
        8 => 60.0,
        _ => return None,
    })
}

/// `chroma_format`, Table 6-5. Code 0 is reserved. No defined profile permits
/// 4:4:4, but the field can express it, so it is named rather than refused.
pub(crate) fn chroma_format(v: u8) -> Option<&'static str> {
    Some(match v {
        1 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => return None,
    })
}

/// `profile_and_level_indication` as `Profile@Level`, Tables 8-1 to 8-4.
///
/// Bit 7 is the escape bit. Below it, bits [6:4] are the profile and bits [3:0]
/// the level; at or above it the whole byte is a flat lookup into Table 8-4.
/// Reserved combinations yield `None` rather than a guess.
///
/// Two rows are worth knowing about. Level `0010` is **HighP**, added by H.262
/// Amd.3 (03/2009) for 1080p50/60; a table without it prints `0x42` (Main@HighP)
/// and `0x12` (High@HighP) as unknown. And the escape table spells its 1440-line
/// level without a space while the level table spells it with one, so each is
/// reproduced as its own table writes it.
pub fn profile_level_label(pli: u8) -> Option<String> {
    if pli & 0x80 != 0 {
        return Some(
            match pli {
                0x82 => "4:2:2@High",
                0x85 => "4:2:2@Main",
                0x8A => "Multi-view@High",
                0x8B => "Multi-view@High1440",
                0x8D => "Multi-view@Main",
                0x8E => "Multi-view@Low",
                _ => return None,
            }
            .to_string(),
        );
    }
    let profile = match (pli >> 4) & 0x07 {
        1 => "High",
        2 => "Spatially Scalable",
        3 => "SNR Scalable",
        4 => "Main",
        5 => "Simple",
        _ => return None,
    };
    let level = match pli & 0x0F {
        2 => "HighP",
        4 => "High",
        6 => "High 1440",
        8 => "Main",
        10 => "Low",
        _ => return None,
    };
    Some(format!("{profile}@{level}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_codes_read_per_codec_and_reserved_fills_nothing() {
        // MPEG-2 (a sequence extension follows): Table 6-3 — 1 is square
        // *pixels*, 2..4 are *display* ratios, the rest reserved.
        let mut h = M2V_HEAD.to_vec();
        for (code, par, dar) in [
            (0x10u8, Some((1, 1)), None),
            (0x20, None, Some((4, 3))),
            (0x30, None, Some((16, 9))),
            (0x40, None, Some((221, 100))),
            (0x50, None, None), // reserved
        ] {
            h[7] = code | 0x03; // keep frame_rate_code 3
            let s = parse_sequence(&h).expect("parses");
            assert_eq!((s.pixel_aspect, s.display_aspect), (par, dar), "{code:#x}");
        }
        // MPEG-2's scan is affirmative-only. The corpus header's
        // progressive_sequence is set (byte 17 bit 3), which declares:
        assert_eq!(parse_sequence(&M2V_HEAD).unwrap().scan_type, Some("progressive"));
        // Cleared, it merely *permits* interlaced pictures — film-sourced
        // DVDs are progressive under a clear flag and both reference tools
        // say so — so it must fill nothing rather than "interlaced".
        let mut cleared = M2V_HEAD;
        cleared[17] &= !0x08;
        assert_eq!(parse_sequence(&cleared).unwrap().scan_type, None);
    }

    #[test]
    fn mpeg1_pel_aspect_is_the_reference_table_not_ffmpegs() {
        // MPEG-1: no extension, the same 4 bits read the 11172-2 pel table
        // (height/width — the pixel ratio is its inverse). Indices 8 and 12
        // are the disputed pair the format reference resolves to 0.9375 and
        // 1.1250 on geometry and interpolation grounds; ffmpeg's 0.9157 and
        // 1.0950 would be (10000, 9157)/(10000, 10950).
        let mut h = M2V_HEAD[..12].to_vec();
        h.extend_from_slice(&[0x00, 0x00, 0x01, 0xB8, 0x00, 0x08, 0x00, 0x40]);
        for (code, pel) in [(0x80u8, 9_375u32), (0xC0, 11_250), (0x10, 10_000)] {
            h[7] = code | 0x03;
            let s = parse_sequence(&h).expect("parses");
            assert!(!s.is_mpeg2);
            assert_eq!(s.pixel_aspect, Some((10_000, pel)), "{code:#x}");
            // Structural: 11172-2 has no interlace anywhere.
            assert_eq!(s.scan_type, Some("progressive"));
        }
    }

    /// `testfiles/sdr/mpeg2.m2v`, bytes 0..22 verbatim: a 320x240 25 fps
    /// sequence header (offsets 0..11) followed by the sequence extension
    /// ffmpeg writes after it (offsets 12..21). A GOP header starts at 22 in
    /// the real file; the tests that need one append it.
    const M2V_HEAD: [u8; 22] = [
        0x00, 0x00, 0x01, 0xB3, 0x14, 0x00, 0xF0, 0x23, 0xFF, 0xFF, 0xE0, 0x18, 0x00, 0x00, 0x01,
        0xB5, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00,
    ];

    /// The GOP header that follows in every corpus file, which ends the
    /// sequence-level data.
    const GOP: [u8; 8] = [0x00, 0x00, 0x01, 0xB8, 0x00, 0x08, 0x00, 0x40];

    #[test]
    fn corpus_m2v_head_parses() {
        let s = parse_sequence(&M2V_HEAD).expect("sequence header");
        assert_eq!((s.width, s.height), (320, 240));
        assert_eq!(s.fps, Some(25.0));
        assert_eq!(s.chroma, Some("4:2:0"));
        assert_eq!(s.bit_depth, 8);
        assert!(s.is_mpeg2, "a sequence extension is present");
        // ffmpeg writes 0x48 = Main@Main here.
        assert_eq!(s.profile_level.as_deref(), Some("Main@Main"));
        // No sequence_display_extension in this file at all, so colour is
        // genuinely unsignalled and nothing may be filled.
        assert!(s.color.0.primaries.is_none());
        assert!(s.color.0.transfer.is_none());
        assert!(s.color.0.matrix.is_none());
        assert!(s.color.0.range.is_none());
    }

    #[test]
    fn mpeg1_has_no_sequence_extension() {
        // `testfiles/sdr/mpeg1.m1v` verbatim: the same 12-byte sequence header
        // with aspect code 1, then a GOP header rather than an extension. No
        // sequence extension is exactly what makes a stream 11172-2.
        let mut d = Vec::from(&M2V_HEAD[..12]);
        d[7] = 0x13;
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert_eq!((s.width, s.height), (320, 240));
        assert_eq!(s.fps, Some(25.0));
        assert!(!s.is_mpeg2, "no sequence extension means MPEG-1");
        assert_eq!(s.chroma, Some("4:2:0"), "MPEG-1 is always 4:2:0");
        assert_eq!(s.profile_level, None, "MPEG-1 has no profile field");
    }

    /// The corpus cannot exercise the primaries and transfer decode: ffmpeg's
    /// mpeg2video encoder writes `colour_description = 1` but leaves both at
    /// the "unspecified" code 2, setting only the matrix. So the full triple is
    /// covered here, by hand, against the spec's own byte layout.
    #[test]
    fn display_extension_decodes_the_full_cicp_triple() {
        let mut d = Vec::from(&M2V_HEAD[..]);
        // sequence_display_extension: ext id 2, video_format 5 (unspecified),
        // colour_description 1, then BT.709 / BT.709 / BT.709, then the two
        // 14-bit display sizes.
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x2B, 0x01, 0x01, 0x01, 0x14, 0x01, 0xE0]);
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert_eq!(s.color.0.primaries.as_deref(), Some("BT.709"));
        assert_eq!(s.color.0.transfer.as_deref(), Some("BT.709"));
        assert_eq!(s.color.0.matrix.as_deref(), Some("BT.709"));
        assert_eq!(s.color.1.primaries, Some(ColorSource::Stream));
        assert!(s.color.0.range.is_none(), "MPEG-2 signals no range");
        assert_eq!(s.color.1.range, None);
    }

    #[test]
    fn the_corpus_mixed_case_reports_only_the_matrix() {
        // What ffmpeg actually writes: colour_description set, primaries and
        // transfer at the explicit "unspecified" code 2, matrix 1 (BT.709) for
        // the tagged files and 5 (BT.470BG) for the PAL one. The unspecified
        // pair must stay absent and untagged, not be filled.
        for (matrix, label) in [(1u8, "BT.709"), (5, "BT.601 (PAL)")] {
            let mut d = Vec::from(&M2V_HEAD[..]);
            d.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x2B, 0x02, 0x02, matrix]);
            d.extend_from_slice(&[0x05, 0x02, 0x07, 0x80]);
            d.extend_from_slice(&GOP);
            let s = parse_sequence(&d).expect("sequence header");
            assert!(s.color.0.primaries.is_none(), "code 2 declines to say");
            assert!(s.color.0.transfer.is_none(), "code 2 declines to say");
            assert_eq!(s.color.0.matrix.as_deref(), Some(label));
            assert_eq!(s.color.1.primaries, None);
            assert_eq!(s.color.1.matrix, Some(ColorSource::Stream));
        }
    }

    #[test]
    fn a_clear_colour_description_flag_signals_nothing() {
        // The extension is present but `colour_description` is 0, so the three
        // CICP bytes are not there at all and the display sizes start early.
        let mut d = Vec::from(&M2V_HEAD[..]);
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x2A, 0x14, 0x01, 0xE0]);
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert!(s.color.0.matrix.is_none(), "no colour description, no colour");
    }

    #[test]
    fn dimension_extensions_extend_the_header() {
        // The header's fields are 12 bits, so parsing them alone caps a stream
        // at 4095x4095. The sequence extension supplies two more bits each.
        // Offsets 16..21 are the extension: h_ext bit 1 is b[1] bit 0, h_ext
        // bit 0 is b[2] bit 7, and v_ext is b[2] bits [6:5].
        let mut d = Vec::from(&M2V_HEAD[..]);
        d[17] = 0x8A | 0x01; // horizontal_size_extension = 0b10
        d[18] = 0x20; // vertical_size_extension = 0b01
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert_eq!((s.width, s.height), (320 | 2 << 12, 240 | 1 << 12));
    }

    #[test]
    fn a_forbidden_zero_colour_code_reads_as_unsignalled() {
        // H.262 Tables 6-7/6-8/6-9 mark value 0 Forbidden where CICP defines
        // Identity/GBR and RGB. Decoding it through the CICP table would print
        // `"RGB"` as this stream's matrix, and with primaries and transfer also
        // 0 that becomes the whole Color line: a fabricated colour description
        // over a stream that signalled nothing usable.
        let mut d = Vec::from(&M2V_HEAD[..]);
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x2B, 0x00, 0x00, 0x00, 0x14, 0x01, 0xE0]);
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert!(s.color.0.primaries.is_none());
        assert!(s.color.0.transfer.is_none());
        assert!(s.color.0.matrix.is_none(), "a forbidden 0 must not decode as RGB");
        assert_eq!(s.color.1.matrix, None);
    }

    #[test]
    fn a_truncated_sequence_extension_leaves_its_fields_unknown() {
        // The extension is cut by the next start code. It must fail whole:
        // reading six bytes regardless would decode the GOP header's bytes as
        // chroma and profile, and setting `is_mpeg2` before the parse would
        // leave MPEG-1's 4:2:0 constant standing on an MPEG-2 report.
        let mut d = Vec::from(&M2V_HEAD[..16]); // header + the `00 00 01 B5` prefix
        d.extend_from_slice(&[0x14, 0x8A]); // only 2 of the extension's 6 bytes
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert!(!s.is_mpeg2, "an incomplete extension settles nothing");
        assert_eq!(s.chroma, Some("4:2:0"), "falls back to the MPEG-1 constant");
        assert_eq!(s.profile_level, None, "no profile was read");
        // And the same bound on the display extension.
        let mut d = Vec::from(&M2V_HEAD[..]);
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x2B, 0x01]);
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert!(s.color.0.matrix.is_none(), "a cut display extension signals nothing");
    }

    #[test]
    fn a_display_extension_without_a_sequence_extension_is_ignored() {
        // 11172-2 defines no extensions at all, so a `sequence_display_extension`
        // with no `sequence_extension` before it cannot describe an MPEG-1
        // stream. Reading it anyway produced a report that called itself MPEG-1
        // while quoting colour out of an MPEG-2-only structure.
        let mut d = Vec::from(&M2V_HEAD[..12]);
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x2B, 0x01, 0x01, 0x01, 0x14, 0x01, 0xE0]);
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert!(!s.is_mpeg2);
        assert!(s.color.0.matrix.is_none(), "no sequence extension, no MPEG-2 colour");
    }

    #[test]
    fn frame_rate_table_is_the_spec_and_not_ffmpegs() {
        assert_eq!(frame_rate(1), Some(24000.0 / 1001.0));
        assert_eq!(frame_rate(2), Some(24.0));
        assert_eq!(frame_rate(3), Some(25.0));
        assert_eq!(frame_rate(4), Some(30000.0 / 1001.0));
        assert_eq!(frame_rate(5), Some(30.0));
        assert_eq!(frame_rate(6), Some(50.0));
        assert_eq!(frame_rate(7), Some(60000.0 / 1001.0));
        assert_eq!(frame_rate(8), Some(60.0));
        // 0 is forbidden; 9..=15 are reserved. ffmpeg fills 9..=13 with Xing
        // and libmpeg3 economy rates, which the spec does not define.
        assert_eq!(frame_rate(0), None);
        for code in 9..=15 {
            assert_eq!(frame_rate(code), None, "code {code} is reserved");
        }
    }

    #[test]
    fn profile_and_level_table_matches_h262_tables_8_1_to_8_4() {
        // The spec's own shorthand examples.
        assert_eq!(profile_level_label(0x48).as_deref(), Some("Main@Main"));
        assert_eq!(profile_level_label(0x44).as_deref(), Some("Main@High"));
        assert_eq!(profile_level_label(0x58).as_deref(), Some("Simple@Main"));
        assert_eq!(profile_level_label(0x16).as_deref(), Some("High@High 1440"));
        // HighP, added by Amd.3 (03/2009) for 1080p50/60.
        assert_eq!(profile_level_label(0x42).as_deref(), Some("Main@HighP"));
        assert_eq!(profile_level_label(0x12).as_deref(), Some("High@HighP"));
        // The scalable profiles.
        assert_eq!(profile_level_label(0x38).as_deref(), Some("SNR Scalable@Main"));
        assert_eq!(profile_level_label(0x26).as_deref(), Some("Spatially Scalable@High 1440"));
        // Table 8-4's escape rows, verbatim including its spaceless "High1440".
        assert_eq!(profile_level_label(0x82).as_deref(), Some("4:2:2@High"));
        assert_eq!(profile_level_label(0x85).as_deref(), Some("4:2:2@Main"));
        assert_eq!(profile_level_label(0x8A).as_deref(), Some("Multi-view@High"));
        assert_eq!(profile_level_label(0x8B).as_deref(), Some("Multi-view@High1440"));
        assert_eq!(profile_level_label(0x8D).as_deref(), Some("Multi-view@Main"));
        assert_eq!(profile_level_label(0x8E).as_deref(), Some("Multi-view@Low"));
        // Reserved: profile 0/6/7, level 0/1/3/5/7/9/11..15, and every escape
        // value outside those six rows.
        assert_eq!(profile_level_label(0x08), None);
        assert_eq!(profile_level_label(0x68), None);
        assert_eq!(profile_level_label(0x49), None);
        assert_eq!(profile_level_label(0x80), None);
        assert_eq!(profile_level_label(0xFF), None);
    }

    #[test]
    fn chroma_table_names_the_three_defined_formats() {
        assert_eq!(chroma_format(1), Some("4:2:0"));
        assert_eq!(chroma_format(2), Some("4:2:2"));
        assert_eq!(chroma_format(3), Some("4:4:4"));
        assert_eq!(chroma_format(0), None, "code 0 is reserved");
    }

    #[test]
    fn a_422_high_stream_reports_its_chroma() {
        let mut d = Vec::from(&M2V_HEAD[..]);
        d[16] = 0x18; // ext id 1, pli high nibble 8
        d[17] = 0x24; // pli low nibble 2 (= 0x82, escape 4:2:2@High), chroma 2
        d.extend_from_slice(&GOP);
        let s = parse_sequence(&d).expect("sequence header");
        assert_eq!(s.chroma, Some("4:2:2"));
        assert_eq!(s.profile_level.as_deref(), Some("4:2:2@High"));
    }

    #[test]
    fn resyncs_past_leading_bytes_that_are_not_a_sequence_header() {
        // A stream cut mid-GOP: a picture start code and a slice precede the
        // next sequence header. Nothing before it may be mistaken for one.
        let mut d = Vec::from(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x0F, 0xFF, 0xF8][..]);
        d.extend_from_slice(&[0x00, 0x00, 0x01, 0x01, 0xAA, 0xBB, 0xCC, 0xDD]);
        d.extend_from_slice(&M2V_HEAD);
        let s = parse_sequence(&d).expect("sequence header after the cut");
        assert_eq!((s.width, s.height), (320, 240));
    }

    #[test]
    fn implausible_headers_and_truncation_are_refused() {
        assert!(parse_sequence(&[]).is_none());
        assert!(parse_sequence(&[0x00, 0x00, 0x01, 0xB3]).is_none());
        // Truncated inside the fixed eight bytes (they end at offset 12).
        assert!(parse_sequence(&M2V_HEAD[..10]).is_none());
        // Reserved frame rate code.
        let mut d = M2V_HEAD;
        d[7] = 0x29;
        assert!(parse_sequence(&d).is_none());
        // Forbidden aspect code.
        let mut d = M2V_HEAD;
        d[7] = 0x03;
        assert!(parse_sequence(&d).is_none());
        // Marker bit clear.
        let mut d = M2V_HEAD;
        d[10] = 0xDF;
        assert!(parse_sequence(&d).is_none());
        // Zero dimensions.
        let mut d = M2V_HEAD;
        d[4] = 0x00;
        d[5] = 0x00;
        d[6] = 0x00;
        assert!(parse_sequence(&d).is_none());
        // Bytes that are not MPEG at all: an HEVC Annex-B head.
        assert!(parse_sequence(&[0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF]).is_none());
    }
}
