//! VC-1 (SMPTE ST 421) sequence header parsing.
//!
//! Read against **ST 421:2013**, which SMPTE publishes on its own preprint
//! server. That matters: the "VC-9" Committee Draft that circulates freely has a
//! *different* sequence-header layout (it puts `PANSCANFLAG`, `PIC_SIZE_FLAG`
//! and `DISP_SIZE_FLAG` in the sequence header, where the published standard has
//! `MAX_CODED_WIDTH`/`HEIGHT`, `PSF` and `DISPLAY_EXT`), so a parser built from
//! the draft desynchronises on real files.
//!
//! Two profiles, two entirely different carriages:
//!
//! - **Simple and Main** (`PROFILE` 0 and 1, FourCC `WMV3`) put a 32-bit
//!   STRUCT_C in the container's codec-private data. It carries the profile and
//!   nothing else this report wants: **no colour, no dimensions, no frame rate.**
//!   Its five reserved fields have fixed values, so they double as a validity
//!   check and are the only reason a bare four bytes can be trusted at all.
//! - **Advanced** (`PROFILE` 3, FourCC `WVC1`) puts a real sequence header
//!   in-band at start code `0x0000010F`, and every container that carries it
//!   also copies that header into codec-private data — Matroska and AVI inside a
//!   `BITMAPINFOHEADER`'s extradata, MP4 inside the `dvc1` box's
//!   `seqhdr_ephdr`. So the colour description costs no sample reads anywhere.
//!
//! **VC-1's colour fields are not CICP**, which is the single easiest thing to
//! get wrong here. The value spaces are narrower, the defaults are not all 1,
//! and `TRANSFER_CHAR` 8 is BT.1361 where CICP 8 is Linear. ffmpeg treats them
//! as raw CICP passthrough with a whitelist that both drops defined values and
//! admits reserved ones; GStreamer agrees with ffmpeg. This module follows the
//! published tables instead, and translates into CICP codes so the labels come
//! from the one shared decoder rather than a second copy that could drift.
//!
//! VC-1 is also the one codec in this corner of the tree that **does** use
//! emulation prevention (`00 00 03 XX` -> `00 00 XX`), so the payload between
//! start codes is unescaped before any bit is read.

use crate::bits::{ebsp_to_rbsp, BitReader};
use crate::model::{ColorInfo, ColorSource, ColorSources};

/// `PROFILE` in the in-band sequence header (§6.1.1), a 2-bit field. Distinct
/// from the config numbering below — see [`profile_from_config`].
const PROFILE_ADVANCED: u32 = 3;

/// The 4-bit config-box profile numbering (RP 2025-2007 §8.1), which also
/// governs STRUCT_C's own `profile` field per §8.3.
const PROFILE_CONFIG_SIMPLE: u8 = 0;
const PROFILE_CONFIG_MAIN: u8 = 4;
const PROFILE_CONFIG_ADVANCED: u8 = 12;

/// Start-code suffix of the sequence header EBDU.
const SC_SEQUENCE_HEADER: u8 = 0x0F;

/// Cap on how much of a caller's buffer the EBDU search will walk.
///
/// Codec-private data is bounded only by the file: a Matroska CodecPrivate can
/// legally declare a gigabyte. Both the search and `ebsp_to_rbsp` are linear in
/// what they are given, and the unescape *copies* its span, so an uncapped scan
/// turns a malformed CodecPrivate into a whole-file read and a heap allocation
/// that tracks the input one for one — on the **default** path, which is exactly
/// the network-transfer cost the bounded head walks exist to avoid. A real
/// sequence header is tens of bytes; 64 KiB is orders of magnitude of headroom.
const MAX_HEADER_SCAN: usize = 64 << 10;

/// What a VC-1 Advanced Profile sequence header declares.
pub struct SeqInfo {
    pub width: u32,
    pub height: u32,
    /// `FRAMERATENR` over `FRAMERATEDR`, or `FRAMERATEEXP` when
    /// `FRAMERATEIND` is set. `None` when `DISPLAY_EXT` or `FRAMERATE_FLAG` is
    /// clear, and for the reserved table rows.
    pub fps: Option<f64>,
    /// Always `Some("4:2:0")` on a valid stream: `COLORDIFF_FORMAT` defines no
    /// other value.
    pub chroma: Option<&'static str>,
    /// `Advanced@L<n>`. The header states its own level.
    pub profile_level: Option<String>,
    /// The colour description. Always populated: `COLOR_FORMAT_FLAG` clear means
    /// the spec's defaults apply, and those defaults are BT.709 primaries,
    /// BT.709 transfer and **BT.601 matrix** — a mixed combination, and the
    /// common case in real content.
    pub color: (ColorInfo, ColorSources),
}

/// Bit depth. VC-1 signals none and every profile is 8-bit, the same class of
/// statement as MPEG-2's constant depth.
pub const BIT_DEPTH: u8 = 8;

/// Parse an Advanced Profile sequence header out of a buffer that begins with,
/// or contains, the `0x0000010F` start code.
///
/// Handles both the raw EBDU (start code included) and codec-private data with
/// other EBDUs around it, which is what every container actually stores.
pub fn parse_sequence_header(data: &[u8]) -> Option<SeqInfo> {
    // Bounded before anything is scanned; see `MAX_HEADER_SCAN`. This also caps
    // the retry loop below, which would otherwise unescape once per candidate
    // across the whole buffer.
    let data = &data[..data.len().min(MAX_HEADER_SCAN)];
    // Every candidate is tried, not just the first. Callers hand this whole
    // config records — an MP4 `dvc1` box opens with seven bytes of profile,
    // capability flags and a rounded frame rate — and those bytes can spell
    // `00 00 01 0F` by coincidence. Retrying costs nothing (a false match fails
    // its own `PROFILE` check immediately) and turns a coincidence from a
    // missed header into a non-event.
    let mut from = 0;
    while let Some(start) = find_ebdu(data, from, SC_SEQUENCE_HEADER) {
        // Bound the payload by the next start code so the entry-point EBDU that
        // follows cannot be read as sequence-header bits, and unescape only that
        // span — stuffing and trailing zeros outside it are not escaped.
        let end = next_start_code(data, start + 4).unwrap_or(data.len());
        if let Some(seq) = data
            .get(start + 4..end)
            .map(ebsp_to_rbsp)
            .and_then(|rbsp| parse_sequence_payload(&rbsp))
        {
            return Some(seq);
        }
        from = start + 1;
    }
    None
}

/// The sequence header body, emulation prevention already removed.
fn parse_sequence_payload(rbsp: &[u8]) -> Option<SeqInfo> {
    let mut r = BitReader::new(rbsp);
    if r.read_bits(2)? != PROFILE_ADVANCED {
        // Simple and Main have no in-band sequence header at all; a 0 or 1 here
        // means the walk is not looking at one.
        return None;
    }
    let level = r.read_bits(3)?;
    // `COLORDIFF_FORMAT`: 1 (4:2:0) is the only defined value, so anything else
    // means either a misaligned walk or a stream no decoder accepts.
    if r.read_bits(2)? != 1 {
        return None;
    }
    r.skip_bits(3)?; // FRMRTQ_POSTPROC — a post-processing indicator, not a rate
    r.skip_bits(5)?; // BITRTQ_POSTPROC
    r.read_bit()?; // POSTPROCFLAG
    let width = (r.read_bits(12)? + 1) * 2;
    let height = (r.read_bits(12)? + 1) * 2;
    r.read_bit()?; // PULLDOWN
    r.read_bit()?; // INTERLACE
    r.read_bit()?; // TFCNTRFLAG
    r.read_bit()?; // FINTERPFLAG
    // RESERVED. Real content sets it — the corpus file does, and ffmpeg skips it
    // without checking — so it is not a marker bit and must not be validated.
    r.read_bit()?;
    r.read_bit()?; // PSF

    let mut fps = None;
    let mut color = None;
    if r.read_bit()? == 1 {
        // DISPLAY_EXT
        r.skip_bits(14)?; // DISP_HORIZ_SIZE
        r.skip_bits(14)?; // DISP_VERT_SIZE
        if r.read_bit()? == 1 && r.read_bits(4)? == 15 {
            r.skip_bits(16)?; // ASPECT_HORIZ_SIZE + ASPECT_VERT_SIZE
        }
        if r.read_bit()? == 1 {
            // FRAMERATE_FLAG
            fps = if r.read_bit()? == 0 {
                frame_rate(r.read_bits(8)?, r.read_bits(4)?)
            } else {
                // FRAMERATEEXP, §6.1.14.4.4.
                Some((r.read_bits(16)? + 1) as f64 / 32.0)
            };
        }
        if r.read_bit()? == 1 {
            // COLOR_FORMAT_FLAG
            color = Some((r.read_bits(8)? as u8, r.read_bits(8)? as u8, r.read_bits(8)? as u8));
        }
    }

    Some(SeqInfo {
        width,
        height,
        fps,
        chroma: Some("4:2:0"),
        profile_level: Some(format!("Advanced@L{level}")),
        color: resolve_color(color),
    })
}

/// Build the colour description from `COLOR_PRIM`/`TRANSFER_CHAR`/`MATRIX_COEF`,
/// or from the spec defaults when `COLOR_FORMAT_FLAG` was clear.
///
/// ST 421:2013 §6.1.14.5: "If COLOR_FORMAT_FLAG == 0, no color format
/// information is present in the bitstream, and these syntax elements shall be
/// set to the default values specified below." The defaults are **1, 1 and 6** —
/// BT.709 primaries and transfer over a **BT.601 matrix**. All three real VC-1
/// files in the corpus clear the flag, so this is the ordinary path, not an edge
/// case, and it is why ffprobe reports VC-1 colour as unknown where this reports
/// a description.
fn resolve_color(signalled: Option<(u8, u8, u8)>) -> (ColorInfo, ColorSources) {
    let (p, t, m, src) = match signalled {
        Some((p, t, m)) => (p, t, m, ColorSource::Stream),
        None => (1, 1, 6, ColorSource::Spec),
    };
    crate::container::color_from_cicp(
        primaries_to_cicp(p),
        transfer_to_cicp(t),
        matrix_to_cicp(m),
        // VC-1 signals no range at all; limited is implied but not stated, so a
        // container-supplied range keeps its own value and provenance.
        None,
        src,
    )
}

/// `COLOR_PRIM` (Table 10) to the equivalent CICP code.
///
/// The defined set is {1, 2, 5, 6} and each of those means what the same CICP
/// code means, so this is a narrowing rather than a remapping: CICP's 4 and 7 are
/// SMPTE-reserved here, and admitting them would name a primary set VC-1 cannot
/// express. Undefined values become CICP 2 (unspecified), which reads as
/// unsignalled rather than as a wrong name.
fn primaries_to_cicp(v: u8) -> u16 {
    match v {
        1 => 1, // BT.709-5 / ST 274 / BT.1361 / ST 296
        5 => 5, // BT.1700 Part B 625 (PAL)
        6 => 6, // SMPTE C, from BT.1700 Part B 525
        _ => 2,
    }
}

/// `TRANSFER_CHAR` (Table 11) to the equivalent CICP code.
///
/// **Row 8 is the trap this function exists for.** VC-1's 8 is "BT.1361
/// Conventional Colour Space"; CICP's 8 is Linear. A passthrough labels a
/// BT.1361 stream as linear-light, which is a materially wrong statement about
/// the curve, so 8 maps to CICP 12 (BT.1361 extended) instead. The other defined
/// rows — 1, 4, 5 and 6 — do coincide with their CICP codes. ffmpeg's whitelist
/// drops 4, 5, 6 and 8 outright and admits 7, which is SMPTE-reserved.
///
/// One caveat on row 8 itself: ST 421 calls it "BT.1361 **Conventional** Colour
/// Space" while H.273's 12 is "BT.1361 **extended** colour gamut". The two are
/// halves of the same recommendation, and 12 is the only BT.1361 code point
/// CICP has, so it is the closest true statement available — certainly closer
/// than Linear. No source consulted settles whether conventional BT.1361 would
/// be better served by CICP 1, whose curve it shares.
fn transfer_to_cicp(v: u8) -> u16 {
    match v {
        1 => 1,  // BT.709-5
        4 => 4,  // BT.1700 Part A, gamma 2.2
        5 => 5,  // BT.1700 Parts B and C, gamma 2.8
        6 => 6,  // BT.1700 Part A, formula
        8 => 12, // BT.1361 — NOT CICP 8, which is Linear
        _ => 2,
    }
}

/// `MATRIX_COEF` (Table 12) to the equivalent CICP code.
///
/// Defined set {1, 2, 6}, both meanings identical to the same CICP codes. Note
/// the default is **6**, not 1 — stated in its own subsection of §6.1.14.5 and
/// applied in [`resolve_color`], not here.
fn matrix_to_cicp(v: u8) -> u16 {
    match v {
        1 => 1, // BT.709-5
        6 => 6, // BT.1700 / BT.601-5 / SMPTE 293M
        _ => 2,
    }
}

/// `FRAMERATENR` (Table 8) over `FRAMERATEDR` (Table 9).
///
/// Both tables forbid 0 and reserve everything past their defined rows, so a
/// pair outside `1..=7` over `1..=2` yields no rate rather than a computed one.
/// Rows 6 and 7 (48000 and 72000) are absent from the VC-9 Committee Draft,
/// which stops at 5; the published standard and the 2005 pre-publication draft
/// both carry all eight.
fn frame_rate(nr: u32, dr: u32) -> Option<f64> {
    let numerator = match nr {
        1 => 24000.0,
        2 => 25000.0,
        3 => 30000.0,
        4 => 50000.0,
        5 => 60000.0,
        6 => 48000.0,
        7 => 72000.0,
        _ => return None,
    };
    let denominator = match dr {
        1 => 1000.0,
        2 => 1001.0,
        _ => return None,
    };
    Some(numerator / denominator)
}

/// STRUCT_C (Annex J.2, Table 263/264): the 32 bits Simple and Main profile put
/// in codec-private data. Returns its `profile` nibble once the structure has
/// validated, for [`profile_from_config`] to name.
///
/// The profile is all it carries that the report wants — the dimensions live in
/// STRUCT_A and the frame rate in STRUCT_B, neither of which is present in the
/// four-byte form. **Read big-endian**: Annex J states that every structure
/// except STRUCT_C is serialized little-endian, and STRUCT_C is the exception.
///
/// **The nibble uses the config box's 0/4/12, not the bitstream's 0/1/3**, which
/// is the opposite of what a field named `profile` sitting inside a *sequence
/// header* structure suggests. RP 2025-2007 §8.3 settles it outright: "profile:
/// shall be set to the same value as is used for the profile field in
/// VC1DecSpecStruc (see section 8.1)", and §8.1 is the 0/4/12 list. Reading it
/// as a bitstream profile makes a Main-profile track — nibble 4 — fall off the
/// end of the match, which takes the whole `dvc1` parse with it and leaves the
/// track with no profile and no bit depth at all.
///
/// The four reserved fields have mandated values (reserved1 = 0, reserved2 = 1,
/// reserved3 = 0, reserved4 = 1; RP 2025 §8.3 lists the same constants Annex J
/// does). They are checked, because four bytes with no magic and no length are
/// otherwise indistinguishable from any other four bytes, and this runs on
/// codec-private data a container merely *claims* is VC-1.
pub fn parse_struct_c(data: &[u8]) -> Option<u8> {
    let b = data.get(..4)?;
    let profile = b[0] >> 4;
    if profile == PROFILE_CONFIG_ADVANCED {
        // Table 264: PROFILE(4) then 28 reserved bits, all zero.
        let reserved = u32::from_be_bytes([b[0] & 0x0F, b[1], b[2], b[3]]);
        return (reserved == 0).then_some(profile);
    }
    let mut r = BitReader::new(b);
    r.skip_bits(4)?; // PROFILE, already read
    r.skip_bits(3)?; // FRMRTQ_POSTPROC
    r.skip_bits(5)?; // BITRTQ_POSTPROC
    r.read_bit()?; // LOOPFILTER
    if r.read_bit()? != 0 {
        return None; // Reserved3
    }
    r.read_bit()?; // MULTIRES
    if r.read_bit()? != 1 {
        return None; // Reserved4
    }
    r.read_bit()?; // FASTUVMC
    r.read_bit()?; // EXTENDED_MV
    r.skip_bits(2)?; // DQUANT
    r.read_bit()?; // VSTRANSFORM
    if r.read_bit()? != 0 {
        return None; // Reserved5
    }
    r.read_bit()?; // OVERLAP
    r.read_bit()?; // SYNCMARKER
    r.read_bit()?; // RANGERED
    r.skip_bits(3)?; // MAXBFRAMES
    r.skip_bits(2)?; // QUANTIZER
    r.read_bit()?; // FINTERPFLAG
    if r.read_bit()? != 1 {
        return None; // reserved4
    }
    // Simple and Main only. RP 2025 §8.1 reserves every value but 0, 4 and 12,
    // and 12 took the branch above, so anything else here is a reserved
    // encoding — which on codec-private data of unknown provenance is far more
    // likely to mean "these are not VC-1 bytes" than a stream worth naming.
    matches!(profile, PROFILE_CONFIG_SIMPLE | PROFILE_CONFIG_MAIN).then_some(profile)
}

/// The profile a 4-bit config-box `profile` field names, per SMPTE RP 2025-2007
/// §8.1: "It shall be 0 for VC-1 Simple profile, 4 for VC-1 Main profile, and 12
/// for VC-1 Advanced profile. All other values are SMPTE reserved."
///
/// The one place that numbering lives. It governs both `VC1DecSpecStruc.profile`
/// and, per §8.3, STRUCT_C's own `profile` — never the in-band sequence header's
/// 2-bit `PROFILE`, which uses 0/1/3 for the same three profiles.
pub fn profile_from_config(code: u8) -> Option<&'static str> {
    Some(match code {
        PROFILE_CONFIG_SIMPLE => "Simple",
        PROFILE_CONFIG_MAIN => "Main",
        PROFILE_CONFIG_ADVANCED => "Advanced",
        _ => return None,
    })
}

/// The config box's `level` field, per RP 2025-2007 §8.1: Simple profile admits
/// 0 (Low) and 2 (Medium); Main adds 4 (High); Advanced takes 0 through 4,
/// meaning L0 through L4. Reserved values yield no level rather than a guess.
///
/// Only reached when the sequence header did not state a level itself, which for
/// Advanced Profile it almost always does.
fn level_from_config(profile: u8, level: u8) -> Option<String> {
    Some(match (profile, level) {
        (PROFILE_CONFIG_SIMPLE | PROFILE_CONFIG_MAIN, 0) => "Low".to_string(),
        (PROFILE_CONFIG_SIMPLE | PROFILE_CONFIG_MAIN, 2) => "Medium".to_string(),
        (PROFILE_CONFIG_MAIN, 4) => "High".to_string(),
        (PROFILE_CONFIG_ADVANCED, l) if l <= 4 => format!("L{l}"),
        _ => return None,
    })
}

/// The `dvc1` box (`VC1DecSpecStruc`), VC-1's `avcC` analogue in ISO base media
/// files. SMPTE RP 2025-2007, "VC-1 Bitstream Storage in the ISO Base Media File
/// Format".
pub struct Dvc1 {
    /// `Advanced@L<n>` for Advanced Profile, or the STRUCT_C profile name.
    pub profile_level: Option<String>,
    /// The Advanced Profile sequence header from `seqhdr_ephdr`, when present.
    pub seq: Option<SeqInfo>,
}

// The box also carries a 32-bit `framerate` — "the rounded frame rate (fps) of
// the track", with `0xFFFFFFFF` for unknown. It is deliberately not returned:
// the only container that has this box is ISO base media, whose own sample
// timing gives the exact rate, and a rounded integer cannot express 24000/1001.
// Skipping past it is all that is needed.

/// Parse a `dvc1` payload.
///
/// **The config box's profile numbering is not the bitstream's**: here 0, 4 and
/// 12 mean Simple, Main and Advanced (and so, per §8.3, inside STRUCT_C), while
/// the in-band sequence header's `PROFILE` uses 0, 1 and 3 for the same three.
/// Mapping one onto the other silently mislabels every VC-1 track in an MP4,
/// which is why [`profile_from_config`] is the only place either value set is
/// spelled out.
pub fn parse_dvc1(payload: &[u8]) -> Option<Dvc1> {
    let first = *payload.first()?;
    let profile = first >> 4;
    let name = profile_from_config(profile)?;
    let level = (first >> 1) & 0x07;
    let rest = payload.get(1..)?;
    if profile == PROFILE_CONFIG_ADVANCED {
        // VC1AdvDecSpecStruc: level(3) cbr(1) reserved1(6) five capability
        // flags(5) reserved2(1) = 16 bits, then framerate(32), then the
        // sequence-header and entry-point EBDUs.
        let seq = rest.get(6..).and_then(parse_sequence_header);
        return Some(Dvc1 {
            // The sequence header states the level itself and is the same field
            // the rest of this module reports, so prefer it when it parsed.
            profile_level: seq.as_ref().and_then(|s| s.profile_level.clone()).or_else(|| {
                Some(match level_from_config(profile, level) {
                    Some(l) => format!("{name}@{l}"),
                    None => name.to_string(),
                })
            }),
            seq,
        });
    }
    // Simple and Main: STRUCT_C (4 bytes) then STRUCT_B (12 bytes). STRUCT_C is
    // validated rather than trusted, and its own profile nibble wins over the
    // box's — both carry the same value set, and §8.3 requires them to agree,
    // so a disagreement means one of the two is not what it claims to be.
    let inner = parse_struct_c(rest)?;
    let name = profile_from_config(inner)?;
    Some(Dvc1 {
        profile_level: Some(match level_from_config(inner, level) {
            Some(l) => format!("{name}@{l}"),
            None => name.to_string(),
        }),
        seq: None,
    })
}

/// Index of an EBDU with the given start-code suffix, at or after `from`.
fn find_ebdu(data: &[u8], from: usize, suffix: u8) -> Option<usize> {
    let mut i = from;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 && data[i + 3] == suffix {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Index of the next `00 00 01` prefix at or after `from`.
///
/// Three bytes are read, so three are required — unlike [`find_ebdu`], which
/// also reads the suffix byte. A buffer ending in exactly `00 00 01` really does
/// end an EBDU there, and bounding at `i + 3` would miss it.
fn next_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sequence-header and entry-point EBDUs from `testfiles/sdr/vc1_advanced.mkv`'s
    /// CodecPrivate, verbatim — bytes 40.. of its `BITMAPINFOHEADER`. 1080p
    /// Advanced Profile at 24000/1001.
    ///
    /// Note the **four**-byte start code `00 00 00 01`: the standard's prefix is
    /// three bytes and the leading zero is legal padding, so the EBDU search has
    /// to find `00 00 01` wherever it sits rather than at offset 0.
    const WVC1_EXTRADATA: [u8; 32] = [
        0x00, 0x00, 0x00, 0x01, 0x0F, 0xDB, 0x7E, 0x3B, 0xF2, 0x1B, 0x8A, 0x3B, 0xF8, 0x86, 0xF1,
        0x80, 0x49, 0x0A, 0x2C, 0x2C, 0x17, 0x27, 0x04, 0x00, 0x00, 0x01, 0x0E, 0x5A, 0xDF, 0xF8,
        0x40, 0x00,
    ];

    #[test]
    fn a_real_advanced_sequence_header_decodes_end_to_end() {
        let s = parse_sequence_header(&WVC1_EXTRADATA).expect("sequence header parses");
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(s.profile_level.as_deref(), Some("Advanced@L3"));
        assert_eq!(s.chroma, Some("4:2:0"));
        // FRAMERATENR 1 over FRAMERATEDR 2. A one-bit misalignment anywhere
        // above would make this garbage, so it is the end-to-end check.
        assert_eq!(s.fps, Some(24000.0 / 1001.0));
    }

    #[test]
    fn a_clear_color_format_flag_takes_the_mixed_spec_defaults() {
        // The corpus file clears COLOR_FORMAT_FLAG, as do all three real VC-1
        // files. The defaults are BT.709 primaries and transfer over a **BT.601
        // matrix** — the mixed combination that makes this worth a test.
        let (color, src) = parse_sequence_header(&WVC1_EXTRADATA).unwrap().color;
        assert_eq!(color.primaries.as_deref(), Some("BT.709"));
        assert_eq!(color.transfer.as_deref(), Some("BT.709"));
        assert_eq!(color.matrix.as_deref(), Some("BT.601 (NTSC)"));
        assert_eq!(src.primaries, Some(ColorSource::Spec));
        assert_eq!(src.matrix, Some(ColorSource::Spec));
        // VC-1 has no range field; the container keeps that one.
        assert_eq!(color.range, None);
        assert_eq!(src.range, None);
    }

    #[test]
    fn transfer_8_is_bt1361_not_linear() {
        // The one row where a CICP passthrough is actively wrong.
        assert_eq!(transfer_to_cicp(8), 12);
        assert_eq!(crate::container::cicp_transfer(12), Some("BT.1361"));
        assert_ne!(crate::container::cicp_transfer(12), crate::container::cicp_transfer(8));
        // The rows that do coincide.
        for v in [1u8, 4, 5, 6] {
            assert_eq!(transfer_to_cicp(v), v as u16, "transfer {v}");
        }
        // ffmpeg's whitelist admits 7; ST 421 reserves it.
        assert_eq!(transfer_to_cicp(7), 2);
    }

    #[test]
    fn the_defined_colour_sets_are_narrower_than_cicp() {
        // Primaries: {1, 5, 6}. CICP defines 4 and 7; VC-1 reserves them.
        for v in [1u8, 5, 6] {
            assert_eq!(primaries_to_cicp(v), v as u16);
        }
        for v in [0u8, 3, 4, 7, 9] {
            assert_eq!(primaries_to_cicp(v), 2, "primaries {v}");
        }
        // Matrix: {1, 6}. 7 is SMPTE-reserved here and ffmpeg admits it.
        assert_eq!(matrix_to_cicp(1), 1);
        assert_eq!(matrix_to_cicp(6), 6);
        for v in [0u8, 3, 4, 5, 7, 9] {
            assert_eq!(matrix_to_cicp(v), 2, "matrix {v}");
        }
    }

    #[test]
    fn frame_rate_table_rows_and_their_guards() {
        assert_eq!(frame_rate(1, 2), Some(24000.0 / 1001.0));
        assert_eq!(frame_rate(2, 1), Some(25.0));
        assert_eq!(frame_rate(3, 2), Some(30000.0 / 1001.0));
        // Rows 6 and 7 exist in the published standard; the Committee Draft
        // stops at 5, so a parser built from it would drop these.
        assert_eq!(frame_rate(6, 1), Some(48.0));
        assert_eq!(frame_rate(7, 1), Some(72.0));
        // Forbidden and reserved.
        assert_eq!(frame_rate(0, 1), None);
        assert_eq!(frame_rate(8, 1), None);
        assert_eq!(frame_rate(1, 0), None);
        assert_eq!(frame_rate(1, 3), None);
    }

    #[test]
    fn a_simple_profile_header_is_not_an_advanced_one() {
        // PROFILE 0 in the top two bits: Simple and Main carry no in-band
        // sequence header, so bytes that begin this way are something else.
        let mut d = WVC1_EXTRADATA;
        d[5] &= 0x3F;
        assert!(parse_sequence_header(&d).is_none());
    }

    #[test]
    fn an_undefined_colordiff_format_is_refused() {
        // COLORDIFF_FORMAT 1 (4:2:0) is the only defined value; anything else
        // means the walk is misaligned and every field after it is noise.
        let mut d = WVC1_EXTRADATA;
        // Bits 5..6 of the first payload byte, which sits at index 5.
        d[5] = (d[5] & !0x06) | 0x04; // -> 0b10
        assert!(parse_sequence_header(&d).is_none());
    }

    #[test]
    fn truncation_declines_rather_than_inventing_a_header() {
        for cut in 0..WVC1_EXTRADATA.len() {
            let s = parse_sequence_header(&WVC1_EXTRADATA[..cut]);
            if let Some(s) = s {
                // Any prefix that does parse must still agree with the whole.
                assert_eq!((s.width, s.height), (1920, 1080), "prefix of {cut} bytes");
            }
        }
    }

    #[test]
    fn emulation_prevention_is_removed_before_the_bits_are_read() {
        // The corpus file's own payload happens to contain no `00 00` pair, so
        // it cannot exercise this at all. Build one that must be escaped:
        // PROFILE 3, LEVEL 3, COLORDIFF 1, MAX_CODED_WIDTH 0 and HEIGHT 1
        // (2x4 pixels), everything else clear, which lays out as
        //
        //   DA 00 00 00 01 00
        //
        // — a `00 00 00` an encoder must escape, and a `00 00 01` it must
        // escape or the byte stream would carry a false start code.
        let raw = [0xDAu8, 0x00, 0x00, 0x00, 0x01, 0x00];
        let escaped = [0xDAu8, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00];
        assert_eq!(ebsp_to_rbsp(&escaped), raw, "the fixture is a real escaping of the payload");
        assert!(!escaped.windows(3).any(|w| w == [0, 0, 1]), "no false start code survives");

        let mut d = vec![0x00, 0x00, 0x01, SC_SEQUENCE_HEADER];
        d.extend_from_slice(&escaped);
        let s = parse_sequence_header(&d).expect("escaped header parses");
        assert_eq!((s.width, s.height), (2, 4));

        // And the parse really does depend on the unescape: reading the escaped
        // bytes directly puts the `03` inside MAX_CODED_HEIGHT and yields 1538.
        assert_eq!(parse_sequence_payload(&escaped).unwrap().height, 1538);
    }

    #[test]
    fn struct_c_validates_its_reserved_constants() {
        // reserved1 = 0, reserved2 = 1, reserved3 = 0, reserved4 = 1. Build a
        // Main profile STRUCT_C with every other field zero. The profile nibble
        // is **4**, the config numbering RP 2025 §8.3 requires here — not the
        // bitstream's 1, which this field looks like it should carry.
        let mut bits = String::from("0100"); // profile = 4 (Main)
        bits.push_str("000"); // FRMRTQ_POSTPROC
        bits.push_str("00000"); // BITRTQ_POSTPROC
        bits.push('0'); // LOOPFILTER
        bits.push('0'); // Reserved3
        bits.push('0'); // MULTIRES
        bits.push('1'); // Reserved4
        bits.push('0'); // FASTUVMC
        bits.push('0'); // EXTENDED_MV
        bits.push_str("00"); // DQUANT
        bits.push('0'); // VSTRANSFORM
        bits.push('0'); // Reserved5
        bits.push('0'); // OVERLAP
        bits.push('0'); // SYNCMARKER
        bits.push('0'); // RANGERED
        bits.push_str("000"); // MAXBFRAMES
        bits.push_str("00"); // QUANTIZER
        bits.push('0'); // FINTERPFLAG
        bits.push('1'); // Reserved6
        let good = pack(&bits);
        assert_eq!(good, [0x40, 0x01, 0x00, 0x01]);
        assert_eq!(parse_struct_c(&good), Some(PROFILE_CONFIG_MAIN));
        assert_eq!(profile_from_config(PROFILE_CONFIG_MAIN), Some("Main"));

        // Flip reserved2 to its forbidden value: four arbitrary bytes must not
        // pass as a codec configuration.
        let mut bad = good.clone();
        bad[1] ^= 0x01;
        assert_eq!(parse_struct_c(&bad), None);

        // The Advanced form is profile 12 plus 28 zero bits.
        assert_eq!(parse_struct_c(&[0xC0, 0, 0, 0]), Some(PROFILE_CONFIG_ADVANCED));
        assert_eq!(parse_struct_c(&[0xC0, 0, 0, 1]), None);
        assert_eq!(parse_struct_c(&[0xC0, 0, 0]), None);

        // Every value §8.1 reserves is refused — including 1 and 3, which would
        // be Main and Advanced if this field used the bitstream numbering.
        // Reading it that way is the documented trap, and it is what made a
        // real Main-profile MP4 track report no profile and no bit depth.
        for p in [1u8, 2, 3, 5, 6, 7, 8, 9, 10, 11, 13, 14, 15] {
            let mut d = good.clone();
            d[0] = (p << 4) | (d[0] & 0x0F);
            assert_eq!(parse_struct_c(&d), None, "reserved profile {p}");
        }
    }

    #[test]
    fn the_config_level_field_follows_rp2025_section_8_1() {
        // "For VC-1 Simple profile, level shall be 0 to indicate Low level and
        // 2 to indicate Medium level. For VC-1 Main profile, level shall be 0
        // ... Low, 2 ... Medium and 4 ... High. For VC-1 Advanced profile,
        // level shall take a value from 0 through 4, corresponding to ... L0
        // through L4."
        assert_eq!(level_from_config(PROFILE_CONFIG_SIMPLE, 0).as_deref(), Some("Low"));
        assert_eq!(level_from_config(PROFILE_CONFIG_SIMPLE, 2).as_deref(), Some("Medium"));
        // Simple has no High level; Main does.
        assert_eq!(level_from_config(PROFILE_CONFIG_SIMPLE, 4), None);
        assert_eq!(level_from_config(PROFILE_CONFIG_MAIN, 4).as_deref(), Some("High"));
        for l in 0..=4u8 {
            assert_eq!(
                level_from_config(PROFILE_CONFIG_ADVANCED, l).as_deref(),
                Some(format!("L{l}").as_str())
            );
        }
        // Everything else is reserved and yields no level rather than a guess.
        for l in [1u8, 3, 5, 6, 7] {
            assert_eq!(level_from_config(PROFILE_CONFIG_MAIN, l), None, "main level {l}");
        }
        assert_eq!(level_from_config(PROFILE_CONFIG_ADVANCED, 5), None);
    }

    #[test]
    fn the_config_box_profile_numbering_is_not_the_bitstreams() {
        // 0/4/12 in the config box *and* inside STRUCT_C (RP 2025 §8.3);
        // 0/1/3 only in the in-band sequence header. A Main-profile `dvc1`
        // carries 4 in both places, and level 4 there means High.
        let mut struct_c = vec![0x40, 0x01, 0x00, 0x01];
        struct_c.extend_from_slice(&[0u8; 12]); // STRUCT_B
        let mut d = vec![(PROFILE_CONFIG_MAIN << 4) | (4 << 1)];
        d.extend_from_slice(&struct_c);
        assert_eq!(parse_dvc1(&d).unwrap().profile_level.as_deref(), Some("Main@High"));

        // Config profile 3 is reserved — it is *not* Advanced, which is 12 —
        // so the box is refused outright rather than being read as a bitstream
        // profile number.
        let mut d3 = vec![3 << 4];
        d3.extend_from_slice(&struct_c);
        assert!(parse_dvc1(&d3).is_none());

        // And a box claiming Simple over a STRUCT_C that says Main is not a
        // configuration either half describes.
        let mut mismatch = vec![PROFILE_CONFIG_SIMPLE << 4];
        mismatch.extend_from_slice(&struct_c);
        assert_eq!(parse_dvc1(&mismatch).unwrap().profile_level.as_deref(), Some("Main@Low"));
    }

    #[test]
    fn a_dvc1_box_yields_the_sequence_header_with_no_sample_reads() {
        // profile 12 (Advanced), level 3, then the two-byte flag block, then a
        // rounded framerate of 24, then the real extradata.
        let mut d = vec![(12 << 4) | (3 << 1), 0x00, 0x00];
        d.extend_from_slice(&24u32.to_be_bytes());
        d.extend_from_slice(&WVC1_EXTRADATA);
        let c = parse_dvc1(&d).expect("dvc1 parses");
        let seq = c.seq.expect("seqhdr_ephdr holds the sequence header");
        assert_eq!((seq.width, seq.height), (1920, 1080));
        // The exact rational from the header, not the box's rounded 24.
        assert_eq!(seq.fps, Some(24000.0 / 1001.0));
        assert_eq!(c.profile_level.as_deref(), Some("Advanced@L3"));
    }

    fn pack(bits: &str) -> Vec<u8> {
        let mut out = vec![0u8; bits.len().div_ceil(8)];
        for (i, c) in bits.chars().enumerate() {
            if c == '1' {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        out
    }
}
