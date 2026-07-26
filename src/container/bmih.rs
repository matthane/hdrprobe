//! `BITMAPINFOHEADER`, the Video for Windows description block, and the FourCC
//! table that goes with it.
//!
//! Three containers hand hdrprobe one of these: AVI in its `strf` chunk, ASF in
//! a Stream Properties Object's type-specific data, and **Matroska**, whose
//! `V_MS/VFW/FOURCC` CodecID wraps the whole VfW structure in CodecPrivate.
//! That last one is why this decoder is here rather than in the AVI backend:
//! it is the only carriage VC-1 in Matroska has, so VC-1 support depends on it.
//!
//! The structure is 40 bytes of little-endian fields followed by codec
//! extradata, and the two facts worth stating outright are both traps:
//!
//! - **`biBitCount` is display bits per pixel, never a bit depth.** A 4:2:0
//!   8-bit YUV stream routinely writes 24 or 12 here. It is deliberately not
//!   exposed by this module, so nothing downstream can report it as a depth.
//! - **`biHeight` is signed**, and negative means top-down for uncompressed RGB.
//!   Compressed formats must write it positive, but real files do not always;
//!   the absolute value is the picture height either way.
//!
//! `biCompression` is the format identifier and wins over `strh.fccHandler`,
//! which can disagree with it. Matching is case-insensitive: the same codec ships
//! as `XVID` and `xvid`, `DIVX` and `divx`.

use super::Codec;

/// The fixed part of the structure. Everything past it is codec extradata.
pub(crate) const HEADER_LEN: usize = 40;

pub(crate) struct BitmapInfoHeader {
    pub width: u32,
    pub height: u32,
    /// `biCompression`, the FourCC that names the codec.
    pub compression: [u8; 4],
}

// Codec extradata — an `avcC` record, a VC-1 sequence header, an MPEG-4 Part 2
// header set, or nothing — is whatever follows [`HEADER_LEN`]. It is not
// returned as a slice because the one caller today needs its position inside the
// file rather than its bytes; a backend holding the buffer can take the tail
// directly.

/// Decode a `BITMAPINFOHEADER` from the head of `data`.
///
/// `None` when the buffer is short or `biSize` undercuts the fixed structure —
/// the latter meaning either a `BITMAPCOREHEADER` (12 bytes, pre-VfW, no
/// FourCC) or garbage. Nothing here can be recovered from either.
///
/// Note that `biSize` is *not* used to bound the extradata that follows: it is
/// written inconsistently — the corpus's VC-1 Matroska file declares 71 over a
/// 72-byte CodecPrivate — so the caller's own slice is the reliable end, which
/// is also what ffmpeg does.
pub(crate) fn parse(data: &[u8]) -> Option<BitmapInfoHeader> {
    let head = data.get(..HEADER_LEN)?;
    if u32::from_le_bytes(head[0..4].try_into().ok()?) < HEADER_LEN as u32 {
        return None;
    }
    // `biHeight` is an i32. `unsigned_abs` rather than `abs` because
    // `i32::MIN.abs()` panics, and a declared height of -2147483648 is exactly
    // the sort of thing a malformed file carries.
    let height = i32::from_le_bytes(head[8..12].try_into().ok()?).unsigned_abs();
    Some(BitmapInfoHeader {
        width: u32::from_le_bytes(head[4..8].try_into().ok()?),
        height,
        compression: [head[16], head[17], head[18], head[19]],
    })
}

/// Map a VfW FourCC to a codec, case-insensitively.
///
/// `None` leaves the caller on its own fallback, which for every backend here
/// means [`Codec::Other`] carrying the FourCC verbatim — a track still reports
/// its container-derived dimensions, frame rate and duration that way, so an
/// unknown codec costs the codec name and nothing else.
///
/// Deliberately much narrower than ffmpeg's `ff_codec_bmp_tags`, and narrower
/// than "every codec this build understands". Three groups are held back on
/// purpose:
///
/// - **MJPEG, DV, H.263 and the raw-bitmap families** have no parser here at
///   all, so naming them would add a label without adding a fact.
/// - **The bare FourCC `MPEG`**, which different tools claim for MPEG-1 and for
///   MPEG-2 and which nothing here can separate — unlike the version-bearing
///   spellings beside it, which are unambiguous and are mapped.
///
/// AVC and HEVC *are* here, and were the late addition. They need their NAL
/// framing decided as well as their identity — VfW extradata is a configuration
/// record in some muxes and raw Annex-B in others — so every caller must pair
/// this with [`is_config_record`]. That rule is settled by real files now
/// (`dev/sdr-format-reference.md` §5, and `testfiles/sdr/h264.avi` against
/// `h264_avcc.avi`), which is what unblocked them.
pub(crate) fn codec_from_fourcc(fourcc: &[u8; 4]) -> Option<Codec> {
    let mut upper = *fourcc;
    upper.make_ascii_uppercase();
    Some(match &upper {
        // ISO/IEC 14496-2. The long tail is real: every encoder that ever
        // shipped picked its own FourCC for the same bitstream.
        b"DIVX" | b"DX50" | b"XVID" | b"FMP4" | b"MP4V" | b"MP4S" | b"M4S2" | b"3IV2"
        | b"DIV1" | b"RMP4" | b"SEDG" | b"FVFW" | b"BLZ0" | b"ZMP4" | b"UMP4" | b"WV1F"
        | b"DXGM" | b"HDX4" | b"SMP4" | b"M4T3" | b"FFDS" => Codec::Mpeg4Part2,
        // SMPTE ST 421. `WVC1` is Advanced Profile, `WMVA` its pre-standard
        // form, and `WMV3` is Simple/Main — one codec, three identifiers.
        b"WVC1" | b"WMVA" | b"VC-1" | b"WMV3" => Codec::Vc1,
        // Microsoft's pre-standard MPEG-4 variants. Not ISO Part 2: the frame
        // headers differ, so these get a name and no parser.
        b"DIV3" | b"DIV4" | b"DIV5" | b"DIV6" | b"MP43" | b"MPG3" | b"DVX3" | b"AP41"
        | b"COL0" | b"COL1" => Codec::MsMpeg4(3),
        b"MP42" | b"DIV2" => Codec::MsMpeg4(2),
        // `MPG4` is v1, and is not `MP4V`, which is ISO Part 2 above.
        b"MPG4" | b"MP41" => Codec::MsMpeg4(1),
        // ITU-T H.264. `AVC1` is the tag an `ffmpeg -c:v copy` remux out of MP4
        // writes, and it is the one that comes with a configuration record;
        // `H264` is a fresh encode's, carrying Annex-B in the chunks. The
        // framing is decided by [`is_config_record`], never by the FourCC.
        b"H264" | b"X264" | b"AVC1" | b"DAVC" | b"VSSH" | b"SMV2" | b"Q264" | b"V264"
        | b"GAVC" | b"UMSV" | b"INMC" | b"TSHD" => Codec::Avc,
        // ITU-T H.265.
        b"H265" | b"HEVC" | b"HVC1" | b"HEV1" => Codec::Hevc,
        // ISO/IEC 11172-2 and ITU-T H.262. Only the spellings that name their
        // own version are here: `ffmpeg -c:v mpeg2video out.avi` writes `mpg2`,
        // and both reference tools read it as MPEG-2 without hesitating. The
        // bare `MPEG` is excluded because it is genuinely ambiguous (see the
        // table's doc); a stream wearing it keeps the FourCC and its picture
        // facts, which is the honest outcome rather than a coin flip on the
        // codec name.
        // `VCR2`, `DVR ` and `slif` share the family row in the reference's
        // table without a stated version, so they stay out for the same reason
        // `MPEG` does.
        b"MPG1" | b"PIM1" => Codec::Mpeg1,
        b"MPG2" | b"PIM2" | b"EM2V" | b"LMP2" => Codec::Mpeg2,
        _ => return None,
    })
}

/// Whether VfW codec extradata is a decoder configuration record rather than
/// raw Annex-B bytes, which is the only thing that decides an AVC or HEVC
/// track's NAL framing.
///
/// **Both framings are common and the FourCC does not separate them.** A fresh
/// `ffmpeg` encode to AVI writes tag `H264` with no extradata at all and
/// Annex-B start codes in the chunks; `ffmpeg -i x.mp4 -c:v copy out.avi`
/// writes tag `avc1`, a 46-byte `avcC` in `strf`, and 4-byte length-prefixed
/// NALs. The muxer's own start-code guard is tag-gated (`avienc.c` runs
/// `ff_check_h264_startcode` only for `MKTAG('H','2','6','4')`), so the second
/// form is emitted without a warning.
///
/// The test is the first byte: `AVCDecoderConfigurationRecord` and
/// `HEVCDecoderConfigurationRecord` both open with `configurationVersion == 1`,
/// while Annex-B opens with a start code, whose first byte is `0x00`. This is
/// exactly ffmpeg's discriminator in `ff_h264_decode_extradata`. The length
/// prefix size is *not* read here — it sits at a different offset in each
/// record (byte 4 for `avcC`, byte 21 for `hvcC`), so it comes from that
/// record's own parser.
pub(crate) fn is_config_record(extradata: &[u8]) -> bool {
    extradata.first() == Some(&0x01)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `testfiles/sdr/vc1_advanced.mkv`'s CodecPrivate verbatim: a 40-byte
    /// header declaring `WVC1` at 1920x1080, then 32 bytes of VC-1 EBDUs.
    const VC1_VFW: [u8; 72] = [
        0x47, 0x00, 0x00, 0x00, 0x80, 0x07, 0x00, 0x00, 0x38, 0x04, 0x00, 0x00, 0x01, 0x00, 0x18,
        0x00, 0x57, 0x56, 0x43, 0x31, 0x00, 0xEC, 0x5E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x0F,
        0xDB, 0x7E, 0x3B, 0xF2, 0x1B, 0x8A, 0x3B, 0xF8, 0x86, 0xF1, 0x80, 0x49, 0x0A, 0x2C, 0x2C,
        0x17, 0x27, 0x04, 0x00, 0x00, 0x01, 0x0E, 0x5A, 0xDF, 0xF8, 0x40, 0x00,
    ];

    #[test]
    fn a_real_vfw_header_yields_identity_size_and_extradata() {
        let h = parse(&VC1_VFW).expect("parses");
        assert_eq!((h.width, h.height), (1920, 1080));
        assert_eq!(&h.compression, b"WVC1");
        assert_eq!(codec_from_fourcc(&h.compression), Some(Codec::Vc1));
        // `biSize` says 71 over a 72-byte buffer, so bounding the tail by it
        // would drop a byte; the caller's own slice is the end that counts.
        let extradata = &VC1_VFW[HEADER_LEN..];
        assert_eq!(extradata.len(), 32);
        assert_eq!(&extradata[..5], &[0x00, 0x00, 0x00, 0x01, 0x0F]);
        // And the sequence header inside it really is readable from here.
        let seq = crate::vc1::parse_sequence_header(extradata).expect("VC-1 header");
        assert_eq!((seq.width, seq.height), (1920, 1080));
    }

    #[test]
    fn a_negative_height_is_a_top_down_bitmap_not_a_negative_picture() {
        let mut d = VC1_VFW;
        d[8..12].copy_from_slice(&(-1080i32).to_le_bytes());
        assert_eq!(parse(&d).unwrap().height, 1080);
        // The value that makes a naive `abs()` panic.
        d[8..12].copy_from_slice(&i32::MIN.to_le_bytes());
        assert_eq!(parse(&d).unwrap().height, 2_147_483_648);
    }

    #[test]
    fn a_short_or_undersized_header_is_refused() {
        assert!(parse(&VC1_VFW[..39]).is_none());
        let mut d = VC1_VFW;
        // `biSize` 12 is a BITMAPCOREHEADER, which has no FourCC at all.
        d[0..4].copy_from_slice(&12u32.to_le_bytes());
        assert!(parse(&d).is_none());
        // Exactly the fixed structure and no extradata is valid.
        let mut exact = [0u8; HEADER_LEN];
        exact[0..4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        exact[16..20].copy_from_slice(b"XVID");
        let h = parse(&exact).unwrap();
        assert_eq!(codec_from_fourcc(&h.compression), Some(Codec::Mpeg4Part2));
    }

    #[test]
    fn fourcc_matching_is_case_insensitive() {
        for f in [b"XVID", b"xvid", b"XviD"] {
            assert_eq!(codec_from_fourcc(f), Some(Codec::Mpeg4Part2), "{:?}", f);
        }
        assert_eq!(codec_from_fourcc(b"wvc1"), Some(Codec::Vc1));
    }

    #[test]
    fn the_ms_mpeg4_family_keeps_its_version_and_is_not_iso_part_2() {
        assert_eq!(codec_from_fourcc(b"DIV3"), Some(Codec::MsMpeg4(3)));
        // `testfiles/sdr/msmpeg4v2.avi`'s own FourCC.
        assert_eq!(codec_from_fourcc(b"MP42"), Some(Codec::MsMpeg4(2)));
        assert_eq!(codec_from_fourcc(b"MPG4"), Some(Codec::MsMpeg4(1)));
        // The pair that reads alike and means different codecs.
        assert_eq!(codec_from_fourcc(b"MP4V"), Some(Codec::Mpeg4Part2));
        assert_eq!(codec_from_fourcc(b"MPG4"), Some(Codec::MsMpeg4(1)));
    }

    #[test]
    fn an_unmapped_fourcc_declines_rather_than_guessing() {
        // Codecs with no parser in this build, and one that is not a codec at
        // all.
        for f in [b"MJPG", b"dvsd", b"H263", b"\0\0\0\0"] {
            assert_eq!(codec_from_fourcc(f), None, "{:?}", f);
        }
    }

    #[test]
    fn the_mpeg_family_maps_only_where_the_fourcc_names_its_version() {
        // `ffmpeg -c:v mpeg2video out.avi` writes `mpg2`, and both reference
        // tools read it as MPEG-2 — the parser for it has been wired since
        // Phase 1, so declining here cost the whole codec identity.
        assert_eq!(codec_from_fourcc(b"mpg2"), Some(Codec::Mpeg2));
        assert_eq!(codec_from_fourcc(b"mpg1"), Some(Codec::Mpeg1));
        assert_eq!(codec_from_fourcc(b"PIM2"), Some(Codec::Mpeg2));
        // The bare family tag is claimed for both versions by different tools
        // and nothing here separates them, so it keeps the FourCC rather than
        // flipping a coin on the codec name.
        assert_eq!(codec_from_fourcc(b"MPEG"), None);
    }

    #[test]
    fn the_two_h264_avi_tags_map_to_one_codec_and_differ_only_in_framing() {
        // `testfiles/sdr/h264.avi` (fresh encode) and `h264_avcc.avi` (an
        // `-c:v copy` remux out of MP4) carry the same codec under different
        // tags, and the tag is *not* what decides the framing.
        assert_eq!(codec_from_fourcc(b"H264"), Some(Codec::Avc));
        assert_eq!(codec_from_fourcc(b"avc1"), Some(Codec::Avc));
        assert_eq!(codec_from_fourcc(b"HEVC"), Some(Codec::Hevc));
    }

    #[test]
    fn the_framing_test_reads_the_extradata_not_the_fourcc() {
        // `h264_avcc.avi`'s `strf` extradata, first bytes: an avcC opens with
        // configurationVersion 1.
        assert!(is_config_record(&[0x01, 0x64, 0x00, 0x0D, 0xFF, 0xE1]));
        // `h264.avi` carries no extradata at all, and the Annex-B variant that
        // does carry some opens with a start code.
        assert!(!is_config_record(&[]));
        assert!(!is_config_record(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x64]));
    }
}
