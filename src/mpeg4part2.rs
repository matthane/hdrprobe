//! MPEG-4 Part 2 (ISO/IEC 14496-2) visual header parsing — Xvid, DivX, 3ivx and
//! everything else that calls itself "MPEG-4 Visual".
//!
//! Same shape as [`crate::mpeg2`]: a head-only parse of the headers that precede
//! the first coded picture, filling only what the container left unset. Three
//! headers matter, and each is optional in practice:
//!
//! - **VisualObjectSequence** (`B0`) carries `profile_and_level_indication`, the
//!   only place a profile is stated. Many AVI and MP4 muxes start at the video
//!   object instead, so no VOS means no profile — never synthesise one from
//!   `video_object_type_indication`, which names a *tool set*, not a profile.
//! - **VisualObject** (`B5`) carries the colour description and the range bit.
//! - **VideoObjectLayer** (`20`..`2F`) carries dimensions, chroma, depth and the
//!   frame rate.
//!
//! Three things about this bitstream shape the code below.
//!
//! **Colour has spec-defined defaults, unlike MPEG-2.** An absent
//! `video_signal_type()`, or a clear `colour_description`, means BT.709
//! primaries, BT.709 transfer, BT.709 matrix and limited range — a genuine spec
//! fill, tagged [`ColorSource::Spec`]. The range bit is signalled independently
//! of the three CICP bytes, so the two halves can carry different provenance and
//! are built separately below.
//!
//! **The frame rate is usually absent.** It exists only when `fixed_vop_rate` is
//! set, and ffmpeg's encoder writes 0 unconditionally. Reporting
//! `vop_time_increment_resolution` as though it were a rate — which is what
//! ffmpeg's *decoder* surfaces — prints "30000 fps" on ordinary content.
//!
//! **Marker bits are load-bearing.** The VOL is bit-packed with no byte
//! alignment and no emulation prevention, so one missed conditional
//! desynchronises every field after it. Every marker this parser passes is
//! *checked* rather than skipped: they cost nothing to verify and they are the
//! only structural evidence that the walk is still aligned.
//!
//! Not a sniffer. `00 00 01 B3` is an MPEG-4 group-of-VOP header *and* an
//! MPEG-1/2 sequence header, and `B5` is claimed by both, so this must only be
//! run on bytes a container already identified as Part 2.

use crate::bits::BitReader;
use crate::model::{ColorInfo, ColorSource, ColorSources};
use crate::mpeg2::next_start_code;

/// `visual_object_sequence_start_code`.
const VOS_START: u8 = 0xB0;
/// `visual_object_start_code`.
const VISUAL_OBJECT_START: u8 = 0xB5;
/// `vop_start_code`. Every header this module reads precedes the first one.
const VOP_START: u8 = 0xB6;

/// `video_object_layer_shape` (Table 6-16).
const SHAPE_RECTANGULAR: u32 = 0b00;
const SHAPE_BINARY_ONLY: u32 = 0b10;
const SHAPE_GRAYSCALE: u32 = 0b11;

/// `video_object_type_indication` values that reroute the VOL to the studio
/// layout (Simple Studio and Core Studio).
const VO_TYPE_SIMPLE_STUDIO: u32 = 14;
const VO_TYPE_CORE_STUDIO: u32 = 15;

/// How many start codes the scan will look at before giving up. A well-formed
/// stream reaches the VOL within a handful; the cap keeps a chunk of unrelated
/// bytes from turning into a long walk.
const MAX_START_CODES: usize = 64;

/// What an MPEG-4 Part 2 visual sequence declares about itself.
pub struct VisualInfo {
    /// 0 when the VOL is not rectangular, which is the only shape carrying
    /// dimensions. The container keeps authority in that case.
    pub width: u32,
    pub height: u32,
    /// `vop_time_increment_resolution / fixed_vop_time_increment`, and `None`
    /// whenever `fixed_vop_rate` is clear — the common case. See the module doc.
    pub fps: Option<f64>,
    pub chroma: Option<&'static str>,
    /// `bits_per_pixel` when `not_8_bit` is set, else 8. `None` when the field
    /// could not be reached (see the sprite note in [`parse_vol`]).
    pub bit_depth: Option<u8>,
    /// `profile_and_level_indication` as `Profile@Level`; `None` without a VOS
    /// header, and for reserved codes.
    pub profile_level: Option<String>,
    /// The colour description, always populated: the spec defines defaults for
    /// every field, so a Part 2 stream is never colour-silent the way MPEG-2 is.
    pub color: (ColorInfo, ColorSources),
    /// `aspect_ratio_info` (Table 6-12: codes 1..5 are the H.264 Table E-1
    /// values, 15 is the explicit `par_width`:`par_height` pair) — a true
    /// pixel ratio. `None` for the reserved codes and a zero extended pair.
    pub pixel_aspect: Option<(u32, u32)>,
    /// The VOL's `interlaced` flag: set means field-coded macroblocks may
    /// occur — `"interlaced"`; clear means none can — `"progressive"`, a real
    /// declaration unlike MPEG-2's clear `progressive_sequence`.
    pub scan_type: Option<&'static str>,
}

/// Parse the visual headers at the head of `data`.
///
/// `None` when no VideoObjectLayer carrying a picture is found, which is what
/// makes it safe to run over a chunk that may hold no header at all: the VOL is
/// the only element carrying dimensions, so without one there is nothing to
/// report.
///
/// **Dimensions are required, not merely welcome.** MPEG-2 slice start codes run
/// `01`..`AF` and so overlap the VOL range `20`..`2F`, which means an MPEG-2
/// stream mislabelled as Part 2 offers thousands of VOL candidates, and one
/// eventually passes every marker bit — observed on real DVD payload, where it
/// produced a chroma format and a spec-defined BT.709 colour description for a
/// stream that is neither. A VOL describing no picture is the practical
/// signature of a misaligned walk, and one describing a picture is the only kind
/// this function's callers can use, so refusing it costs nothing real: the
/// shapes that carry no dimensions (binary-only, and the non-rectangular forms)
/// carry nothing else the report wants either.
pub fn parse_visual(data: &[u8]) -> Option<VisualInfo> {
    let mut pli = None;
    let mut signal = None;
    let mut vol = None;

    let mut pos = next_start_code(data, 0)?;
    for _ in 0..MAX_START_CODES {
        let next = next_start_code(data, pos + 4);
        // Bound every element by the following start code, so a truncated
        // header declines rather than reading the next element as its own bits.
        let elem_end = next.unwrap_or(data.len());
        let body = data.get(pos + 4..elem_end).unwrap_or(&[]);
        match data[pos + 3] {
            VOS_START => pli = body.first().copied(),
            VISUAL_OBJECT_START => signal = parse_visual_object(body),
            // `video_object_layer_start_code`, one per layer. The first one that
            // parses describes the base layer, which is the reported track.
            0x20..=0x2F => {
                vol = parse_vol(body).filter(|v| v.width != 0 && v.height != 0);
                if vol.is_some() {
                    break;
                }
            }
            // Coded pictures begin: every header above is behind us.
            VOP_START => break,
            _ => {}
        }
        match next {
            Some(n) => pos = n,
            None => break,
        }
    }

    let vol = vol?;
    Some(VisualInfo {
        width: vol.width,
        height: vol.height,
        fps: vol.fps,
        chroma: vol.chroma,
        bit_depth: vol.bit_depth,
        profile_level: pli.and_then(profile_level_label),
        color: signal.unwrap_or_default().resolve(),
        pixel_aspect: vol.pixel_aspect,
        scan_type: vol.scan_type,
    })
}

/// `VisualObject()`'s `video_signal_type()`, as read.
///
/// The default is the spec's own: no `video_signal_type()` element at all, which
/// is exactly what [`Default`] must mean here — every field then falls to its
/// specified default rather than to "unsignalled".
#[derive(Default)]
struct VideoSignal {
    /// `video_range`, and whether the bitstream actually stated it. It is
    /// carried by `video_signal_type()` itself, one level above the
    /// `colour_description` flag, so a stream routinely signals the range and
    /// nothing else.
    full_range: bool,
    range_signalled: bool,
    /// `colour_primaries`, `transfer_characteristics`, `matrix_coefficients`,
    /// present only behind `colour_description`.
    cicp: Option<(u8, u8, u8)>,
}

impl VideoSignal {
    /// Build the colour description and its per-field provenance.
    ///
    /// The two halves are assembled separately because they can disagree about
    /// provenance: a stream that sets `video_signal_type` but clears
    /// `colour_description` signals its range and takes BT.709 by specification.
    fn resolve(&self) -> (ColorInfo, ColorSources) {
        // ISO/IEC 14496-2 §6.3.2: absent, or with `colour_description` clear,
        // these "shall be" 1 / 1 / 1 — the same class of statement as the Dolby
        // Vision compatibility-id fill, and the same tag.
        let (p, t, m, src) = match self.cicp {
            Some((p, t, m)) => (p as u16, t as u16, m as u16, ColorSource::Stream),
            None => (1, 1, 1, ColorSource::Spec),
        };
        let (mut color, mut sources) = crate::container::color_from_cicp(p, t, m, None, src);
        color.range = Some(crate::container::cicp_range(self.full_range).to_string());
        sources.range =
            Some(if self.range_signalled { ColorSource::Stream } else { ColorSource::Spec });
        (color, sources)
    }
}

/// `VisualObject()` (§6.2.2), from the byte after its start code.
///
/// `None` for a non-video visual object (still texture, mesh, face animation),
/// whose payload has a different layout and describes no picture.
fn parse_visual_object(body: &[u8]) -> Option<VideoSignal> {
    let mut r = BitReader::new(body);
    if r.read_bit()? == 1 {
        // visual_object_verid(4) + visual_object_priority(3).
        r.skip_bits(7)?;
    }
    // `visual_object_type`: 1 video, 2 still texture. Only those two carry a
    // `video_signal_type()`.
    let vo_type = r.read_bits(4)?;
    if vo_type != 1 && vo_type != 2 {
        return None;
    }
    let mut signal = VideoSignal::default();
    if r.read_bit()? == 0 {
        return Some(signal);
    }
    r.skip_bits(3)?; // video_format
    signal.full_range = r.read_bit()? == 1;
    signal.range_signalled = true;
    if r.read_bit()? == 1 {
        signal.cicp =
            Some((r.read_bits(8)? as u8, r.read_bits(8)? as u8, r.read_bits(8)? as u8));
    }
    Some(signal)
}

/// The VideoObjectLayer fields this module reads.
struct VolInfo {
    width: u32,
    height: u32,
    fps: Option<f64>,
    chroma: Option<&'static str>,
    bit_depth: Option<u8>,
    pixel_aspect: Option<(u32, u32)>,
    scan_type: Option<&'static str>,
}

/// `VideoObjectLayer()` (§6.2.3), from the byte after its start code.
///
/// Every marker bit on the path is verified. `None` on any that reads 0, on a
/// forbidden `vop_time_increment_resolution` of zero, and on truncation — all
/// three mean the walk is no longer aligned, and a misaligned walk produces
/// plausible-looking dimensions out of unrelated bits.
fn parse_vol(body: &[u8]) -> Option<VolInfo> {
    let mut r = BitReader::new(body);
    r.read_bit()?; // random_accessible_vol
    let vo_type = r.read_bits(8)?;
    if vo_type == VO_TYPE_SIMPLE_STUDIO || vo_type == VO_TYPE_CORE_STUDIO {
        return parse_studio_vol(&mut r);
    }

    // `video_object_layer_verid` defaults to 1 when the identifier is absent,
    // and gates two later layout decisions, so it must be carried rather than
    // skipped.
    let verid = if r.read_bit()? == 1 {
        let v = r.read_bits(4)?;
        r.skip_bits(3)?; // video_object_layer_priority
        v
    } else {
        1
    };

    let pixel_aspect = match r.read_bits(4)? {
        0b1111 => {
            // Extended PAR: the explicit pair.
            let w = r.read_bits(8)?;
            let h = r.read_bits(8)?;
            (w > 0 && h > 0).then_some((w, h))
        }
        // Codes 1..5 are Table 6-12's defined set, numerically the same
        // ratios as H.264 Table E-1's first five rows.
        code @ 1..=5 => crate::hevc::sps::sar_from_idc(code),
        _ => None,
    };

    // Outside the studio profiles 4:2:0 is the only value `chroma_format` may
    // take, so it is a constant of the format in the same sense as MPEG-1's —
    // and `vol_control_parameters` is optional, so most streams never state it.
    // A stream that states something else is malformed; report nothing rather
    // than a chroma format no profile defines.
    let mut chroma = Some("4:2:0");
    if r.read_bit()? == 1 {
        chroma = if r.read_bits(2)? == 1 { Some("4:2:0") } else { None };
        r.read_bit()?; // low_delay
        if r.read_bit()? == 1 {
            read_vbv_parameters(&mut r)?;
        }
    }

    let shape = r.read_bits(2)?;
    if shape == SHAPE_GRAYSCALE && verid != 1 {
        r.skip_bits(4)?; // video_object_layer_shape_extension
    }

    marker(&mut r)?;
    let resolution = r.read_bits(16)?;
    if resolution == 0 {
        return None; // forbidden by §6.3.3
    }
    marker(&mut r)?;
    let fps = if r.read_bit()? == 1 {
        let increment = r.read_bits(time_increment_bits(resolution))?;
        // A 16-bit resolution over increment 1 can state 65535 fps; the
        // shared bound applies like every other declared ratio.
        (increment > 0)
            .then(|| resolution as f64 / increment as f64)
            .and_then(crate::container::plausible_fps)
    } else {
        None
    };

    let (mut width, mut height) = (0, 0);
    let mut bit_depth = None;
    let mut scan_type = None;
    if shape != SHAPE_BINARY_ONLY {
        if shape == SHAPE_RECTANGULAR {
            marker(&mut r)?;
            width = r.read_bits(13)?;
            marker(&mut r)?;
            height = r.read_bits(13)?;
            marker(&mut r)?;
        }
        scan_type = Some(if r.read_bit()? == 1 { "interlaced" } else { "progressive" });
        r.read_bit()?; // obmc_disable
        let sprite = r.read_bits(if verid == 1 { 1 } else { 2 })?;
        // A sprite-coded layer inserts a block whose exact field layout this
        // project has no primary source for — `dev/sdr-format-reference.md` §2
        // elides it, and the one implementation consulted was paraphrased rather
        // than transcribed. Guessing it would desynchronise `not_8_bit` and put
        // a fabricated depth on the report, so stop here instead and leave the
        // depth unknown. No encoder in the corpus emits sprites, and ffmpeg's
        // never does.
        if sprite == 0 {
            if verid != 1 && shape != SHAPE_RECTANGULAR {
                r.read_bit()?; // sadct_disable
            }
            bit_depth = if r.read_bit()? == 1 {
                r.skip_bits(4)?; // quant_precision
                let bpp = r.read_bits(4)?;
                // `bits_per_pixel` is a literal depth, not an offset, and the
                // field is wider than the values it may hold.
                (4..=12).contains(&bpp).then_some(bpp as u8)
            } else {
                Some(8)
            };
        }
    }

    Some(VolInfo { width, height, fps, chroma, bit_depth, pixel_aspect, scan_type })
}

/// The studio-profile VOL layout, from the bit after `video_object_type_indication`.
///
/// **Source: ffmpeg's `mpeg4videodec.c::decode_studio_vol_header`, and nothing
/// else.** ISO/IEC 14496-2:2001/Amd 1:2002 is sold with no free preview, the only
/// reachable 14496-2 full text predates the studio profile, and GStreamer
/// recognises the studio profile *codes* without parsing this header — so there
/// is no second witness in existence. Two caveats ride along from that source:
/// ffmpeg decodes only `bit_depth == 10` and rejects the rest, so its 12-bit
/// path is untested code, and the 4-bit width of `bit_depth` is inferred from a
/// shift rather than stated.
///
/// It is implemented anyway because Simple/Core Studio is the *only* Part 2
/// variant that signals a depth above 8 or a chroma format other than 4:2:0.
/// Leaving it unparsed would drop real information on the one variant that
/// carries any.
fn parse_studio_vol(r: &mut BitReader) -> Option<VolInfo> {
    r.skip_bits(4)?; // video_object_layer_verid
    let shape = r.read_bits(2)?;
    r.skip_bits(4)?; // video_object_layer_shape_extension
    r.read_bit()?; // progressive_sequence

    let mut chroma = None;
    let mut bit_depth = None;
    if shape != SHAPE_BINARY_ONLY {
        r.read_bit()?; // rgb_components
        // Table 6-5, shared with MPEG-2 rather than restated. ffmpeg rejects 0
        // and 1 here (a studio layer may not be 4:2:0); this reports what the
        // field says and lets an out-of-profile stream describe itself.
        chroma = crate::mpeg2::chroma_format(r.read_bits(2)? as u8);
        let bd = r.read_bits(4)?;
        bit_depth = (8..=12).contains(&bd).then_some(bd as u8);
    }

    let (mut width, mut height) = (0, 0);
    if shape == SHAPE_RECTANGULAR {
        marker(r)?;
        width = r.read_bits(14)?;
        marker(r)?;
        height = r.read_bits(14)?;
        marker(r)?;
    }

    let pixel_aspect = match r.read_bits(4)? {
        0b1111 => {
            // Extended PAR: the explicit pair.
            let w = r.read_bits(8)?;
            let h = r.read_bits(8)?;
            (w > 0 && h > 0).then_some((w, h))
        }
        // Codes 1..5 are Table 6-12's defined set, numerically the same
        // ratios as H.264 Table E-1's first five rows.
        code @ 1..=5 => crate::hevc::sps::sar_from_idc(code),
        _ => None,
    };
    // `frame_rate_code` follows, and is deliberately **not** decoded. The
    // obvious reading is H.262's Table 6-4 — same field name, same 4-bit width,
    // and the studio profile descends from MPEG-2 — but no reachable source
    // states the studio table, and ffmpeg, the sole witness for the rest of this
    // layout, *skips* this field rather than decoding it, so it cannot be the
    // witness for that mapping either. Read for alignment and dropped: a wrong
    // row here would print a frame rate the stream never stated, which is worse
    // than printing none.
    r.skip_bits(4)?;

    Some(VolInfo { width, height, fps: None, chroma, bit_depth, pixel_aspect, scan_type: None })
}

/// Read one marker bit, failing when it is not 1.
fn marker(r: &mut BitReader) -> Option<()> {
    (r.read_bit()? == 1).then_some(())
}

/// `vbv_parameters()`: three 15-bit halves, a 3-bit and an 11-bit field, and
/// five interleaved marker bits — 79 bits in all. Only its length matters here,
/// but the markers are checked on the way through for the same reason as every
/// other marker in this file.
fn read_vbv_parameters(r: &mut BitReader) -> Option<()> {
    r.skip_bits(15)?; // first_half_bit_rate
    marker(r)?;
    r.skip_bits(15)?; // latter_half_bit_rate
    marker(r)?;
    r.skip_bits(15)?; // first_half_vbv_buffer_size
    marker(r)?;
    r.skip_bits(3)?; // latter_half_vbv_buffer_size
    r.skip_bits(11)?; // first_half_vbv_occupancy
    marker(r)?;
    r.skip_bits(15)?; // latter_half_vbv_occupancy
    marker(r)
}

/// Width of `fixed_vop_time_increment`, `ceil(log2(vop_time_increment_resolution))`.
///
/// Clamped to at least one bit, matching ffmpeg and GStreamer: the spec formula
/// yields zero at a resolution of 1, and a zero-width field has no defined
/// reading. Getting this wrong by one bit desynchronises the dimensions that
/// follow, so it is pinned by a test.
fn time_increment_bits(resolution: u32) -> u32 {
    (32 - (resolution - 1).leading_zeros()).max(1)
}

/// `profile_and_level_indication` as `Profile@Level`, Table G-1.
///
/// Structurally a flat byte table rather than a profile nibble and a level
/// nibble: Simple's levels are not contiguous with its own high nibble (L0 and
/// L0b sit at `0x08`/`0x09`) and Advanced Simple's L3b is at `0xF7`, above L5.
/// Reserved codes yield `None`.
///
/// Provenance is three-tiered, and worth keeping straight because the tiers
/// disagree about how much is really known:
///
/// - **Primary**, from 14496-2:2001/Amd.3:2003, which edits this table directly:
///   Advanced Simple @ L3b = `0xF7` and Simple Scalable @ L0 = `0x10` outright,
///   and its two reserved-range edits pin the neighbourhoods — `0x08` was
///   already assigned, `0xF0`..`0xF5` were already assigned, and `0xF8` upward
///   were already assigned.
/// - **Two witnesses**, MediaInfo and GStreamer's `gstmpeg4parser.c`, agreeing
///   on every remaining block: the studio rows, the ASP block and FGS.
/// - **Single-sourced (MediaInfo only)**: Simple @ L0b/L4a/L5/L6 and Simple
///   Studio @ L5/L6, which postdate the amendment above and come from
///   14496-2:2004/Amd 2:2005 and /Amd 5:2009, neither with a reachable preview.
///   A wrong digit here prints a wrong level, never a wrong codec.
fn profile_level_label(pli: u8) -> Option<String> {
    let (profile, level) = match pli {
        // Simple. Note 0x04 is L4a, not L4: the level list runs
        // L0, L0b, L1, L2, L3, L4a, L5, L6 and has no plain L4.
        0x01 => ("Simple", "1"),
        0x02 => ("Simple", "2"),
        0x03 => ("Simple", "3"),
        0x04 => ("Simple", "4a"),
        0x05 => ("Simple", "5"),
        0x06 => ("Simple", "6"),
        0x08 => ("Simple", "0"),
        0x09 => ("Simple", "0b"),
        0x10 => ("Simple Scalable", "0"),
        0x11 => ("Simple Scalable", "1"),
        0x12 => ("Simple Scalable", "2"),
        0x21 => ("Core", "1"),
        0x22 => ("Core", "2"),
        0x32 => ("Main", "2"),
        0x33 => ("Main", "3"),
        0x34 => ("Main", "4"),
        0x42 => ("N-bit", "2"),
        0xB1 => ("Advanced Coding Efficiency", "1"),
        0xB2 => ("Advanced Coding Efficiency", "2"),
        0xB3 => ("Advanced Coding Efficiency", "3"),
        0xB4 => ("Advanced Coding Efficiency", "4"),
        0xE1 => ("Simple Studio", "1"),
        0xE2 => ("Simple Studio", "2"),
        0xE3 => ("Simple Studio", "3"),
        0xE4 => ("Simple Studio", "4"),
        0xE5 => ("Core Studio", "1"),
        0xE6 => ("Core Studio", "2"),
        0xE7 => ("Core Studio", "3"),
        0xE8 => ("Core Studio", "4"),
        0xEB => ("Simple Studio", "5"),
        0xEC => ("Simple Studio", "6"),
        0xF0 => ("Advanced Simple", "0"),
        0xF1 => ("Advanced Simple", "1"),
        0xF2 => ("Advanced Simple", "2"),
        0xF3 => ("Advanced Simple", "3"),
        0xF4 => ("Advanced Simple", "4"),
        0xF5 => ("Advanced Simple", "5"),
        0xF7 => ("Advanced Simple", "3b"),
        0xF8 => ("Fine Granularity Scalable", "0"),
        0xF9 => ("Fine Granularity Scalable", "1"),
        0xFA => ("Fine Granularity Scalable", "2"),
        0xFB => ("Fine Granularity Scalable", "3"),
        0xFC => ("Fine Granularity Scalable", "4"),
        0xFD => ("Fine Granularity Scalable", "5"),
        _ => return None,
    };
    Some(format!("{profile}@L{level}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `testfiles/sdr/mpeg4p2.mkv`'s CodecPrivate verbatim — VOS, VisualObject,
    /// VideoObject, VOL and a `Lavc` user-data element. Byte-identical to the
    /// `esds` DecoderSpecificInfo in `testfiles/sdr/mpeg4p2.mp4`, so one fixture
    /// covers both carriage paths.
    const XVID_HEADERS: [u8; 47] = [
        0x00, 0x00, 0x01, 0xB0, 0x01, 0x00, 0x00, 0x01, 0xB5, 0x89, 0x13, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x01, 0x20, 0x00, 0xC4, 0x8D, 0x88, 0x00, 0xCD, 0x0A, 0x04, 0x1E, 0x14, 0x43,
        0x00, 0x00, 0x01, 0xB2, 0x4C, 0x61, 0x76, 0x63, 0x36, 0x32, 0x2E, 0x31, 0x31, 0x2E, 0x31,
        0x30, 0x30,
    ];

    #[test]
    fn real_headers_decode_end_to_end() {
        let v = parse_visual(&XVID_HEADERS).expect("VOL parses");
        assert_eq!((v.width, v.height), (320, 240));
        assert_eq!(v.chroma, Some("4:2:0"));
        assert_eq!(v.bit_depth, Some(8));
        assert_eq!(v.profile_level.as_deref(), Some("Simple@L1"));
        // `fixed_vop_rate` is clear, which is ffmpeg's unconditional default.
        // The 25 in the header is `vop_time_increment_resolution`, a tick rate.
        assert_eq!(v.fps, None);
    }

    #[test]
    fn absent_video_signal_type_takes_the_spec_defaults() {
        // The real file's VisualObject clears `video_signal_type`, so all four
        // fields are specified rather than signalled. Every one of them must
        // carry `Spec`, or the report would claim the stream stated them.
        let v = parse_visual(&XVID_HEADERS).unwrap();
        let (color, src) = v.color;
        assert_eq!(color.primaries.as_deref(), Some("BT.709"));
        assert_eq!(color.transfer.as_deref(), Some("BT.709"));
        assert_eq!(color.matrix.as_deref(), Some("BT.709"));
        assert_eq!(color.range.as_deref(), Some("limited"));
        assert_eq!(src.primaries, Some(ColorSource::Spec));
        assert_eq!(src.transfer, Some(ColorSource::Spec));
        assert_eq!(src.matrix, Some(ColorSource::Spec));
        assert_eq!(src.range, Some(ColorSource::Spec));
    }

    /// A `VisualObject` payload with `video_signal_type` set: identifier absent,
    /// type 1 (video), format 5, full range, colour description present with
    /// BT.2020 / BT.709 / BT.2020 non-constant.
    #[test]
    fn signalled_colour_and_range_are_tagged_stream() {
        let mut bits = String::new();
        bits.push('0'); // is_visual_object_identifier
        bits.push_str("0001"); // visual_object_type = 1
        bits.push('1'); // video_signal_type
        bits.push_str("101"); // video_format = 5
        bits.push('1'); // video_range = full
        bits.push('1'); // colour_description
        bits.push_str("00001001"); // colour_primaries = 9
        bits.push_str("00000001"); // transfer_characteristics = 1
        bits.push_str("00001001"); // matrix_coefficients = 9
        let signal = parse_visual_object(&pack(&bits)).unwrap();
        let (color, src) = signal.resolve();
        assert_eq!(color.primaries.as_deref(), Some("BT.2020"));
        assert_eq!(color.transfer.as_deref(), Some("BT.709"));
        assert_eq!(color.matrix.as_deref(), Some("BT.2020 NCL"));
        assert_eq!(color.range.as_deref(), Some("full"));
        assert_eq!(src.primaries, Some(ColorSource::Stream));
        assert_eq!(src.range, Some(ColorSource::Stream));
    }

    /// `video_signal_type` set but `colour_description` clear: the range is a
    /// real signal while the three CICP fields are specified. The two halves
    /// must not share a provenance tag.
    #[test]
    fn a_signalled_range_over_specified_primaries_splits_provenance() {
        let mut bits = String::new();
        bits.push('0'); // is_visual_object_identifier
        bits.push_str("0001"); // visual_object_type = 1
        bits.push('1'); // video_signal_type
        bits.push_str("000"); // video_format
        bits.push('1'); // video_range = full
        bits.push('0'); // colour_description absent
        let (color, src) = parse_visual_object(&pack(&bits)).unwrap().resolve();
        assert_eq!(color.range.as_deref(), Some("full"));
        assert_eq!(src.range, Some(ColorSource::Stream));
        assert_eq!(color.primaries.as_deref(), Some("BT.709"));
        assert_eq!(src.primaries, Some(ColorSource::Spec));
    }

    #[test]
    fn a_non_video_visual_object_is_refused() {
        // `visual_object_type` 3 (mesh) and 4 (face animation) have different
        // payload layouts; reading their bits as a `video_signal_type()` would
        // invent a colour description out of unrelated fields.
        for vo_type in [3u8, 4, 5] {
            let bits = format!("0{vo_type:04b}");
            assert!(parse_visual_object(&pack(&bits)).is_none(), "type {vo_type}");
        }
        // Type 1 (video) and 2 (still texture) both do carry one.
        assert!(parse_visual_object(&pack("00001")).is_some());
        assert!(parse_visual_object(&pack("00010")).is_some());
    }

    #[test]
    fn a_zero_marker_bit_refuses_the_vol() {
        // The real VOL with the marker before `vop_time_increment_resolution`
        // cleared. Everything after it would still decode to *something*; the
        // point is that it must not.
        let mut hdr = XVID_HEADERS;
        // Byte 22 (`0x88`) holds that marker at bit 4.
        hdr[22] &= !0x08;
        assert!(parse_visual(&hdr).is_none());
    }

    #[test]
    fn time_increment_width_matches_the_reference_implementations() {
        // ceil(log2(n)), clamped to one bit. 25 -> 5 is the corpus file's own
        // value; a 4 or a 6 there shifts every following field.
        assert_eq!(time_increment_bits(25), 5);
        assert_eq!(time_increment_bits(16), 4);
        assert_eq!(time_increment_bits(17), 5);
        assert_eq!(time_increment_bits(2), 1);
        assert_eq!(time_increment_bits(1), 1);
        assert_eq!(time_increment_bits(30000), 15);
    }

    #[test]
    fn a_fixed_vop_rate_yields_a_real_frame_rate() {
        // The corpus VOL with `fixed_vop_rate` set and a 5-bit increment of 1,
        // giving 25/1. Rebuilt from bits rather than patched, because setting
        // the flag lengthens the element.
        let mut bits = String::new();
        bits.push('0'); // random_accessible_vol
        bits.push_str("00000001"); // video_object_type_indication = 1
        bits.push('0'); // is_object_layer_identifier -> verid defaults to 1
        bits.push_str("0001"); // aspect_ratio_info = 1
        bits.push('0'); // vol_control_parameters absent
        bits.push_str("00"); // shape = rectangular
        bits.push('1'); // marker
        bits.push_str(&format!("{:016b}", 25)); // vop_time_increment_resolution
        bits.push('1'); // marker
        bits.push('1'); // fixed_vop_rate
        bits.push_str("00001"); // fixed_vop_time_increment, 5 bits
        bits.push('1'); // marker
        bits.push_str(&format!("{:013b}", 320));
        bits.push('1'); // marker
        bits.push_str(&format!("{:013b}", 240));
        bits.push('1'); // marker
        bits.push('0'); // interlaced
        bits.push('1'); // obmc_disable
        bits.push('0'); // sprite_enable (verid 1 -> one bit)
        bits.push('0'); // not_8_bit
        let vol = parse_vol(&pack(&bits)).unwrap();
        assert_eq!((vol.width, vol.height), (320, 240));
        assert_eq!(vol.fps, Some(25.0));
        assert_eq!(vol.bit_depth, Some(8));
    }

    #[test]
    fn not_8_bit_reports_the_signalled_depth() {
        let mut bits = String::new();
        bits.push('0');
        bits.push_str("00000001");
        bits.push('0');
        bits.push_str("0001");
        bits.push('0');
        bits.push_str("00");
        bits.push('1');
        bits.push_str(&format!("{:016b}", 25));
        bits.push('1');
        bits.push('0'); // fixed_vop_rate clear
        bits.push('1');
        bits.push_str(&format!("{:013b}", 720));
        bits.push('1');
        bits.push_str(&format!("{:013b}", 576));
        bits.push('1');
        bits.push('0');
        bits.push('1');
        bits.push('0'); // sprite_enable
        bits.push('1'); // not_8_bit
        bits.push_str("1010"); // quant_precision
        bits.push_str("1010"); // bits_per_pixel = 10
        let vol = parse_vol(&pack(&bits)).unwrap();
        assert_eq!(vol.bit_depth, Some(10));
        assert_eq!((vol.width, vol.height), (720, 576));
    }

    #[test]
    fn a_sprite_layer_declines_to_guess_the_depth() {
        // Identical to the test above except `sprite_enable` is 1. The block it
        // introduces is unsourced, so the depth must go unreported rather than
        // being read from whatever bit lands next.
        let mut bits = String::new();
        bits.push('0');
        bits.push_str("00000001");
        bits.push('0');
        bits.push_str("0001");
        bits.push('0');
        bits.push_str("00");
        bits.push('1');
        bits.push_str(&format!("{:016b}", 25));
        bits.push('1');
        bits.push('0');
        bits.push('1');
        bits.push_str(&format!("{:013b}", 720));
        bits.push('1');
        bits.push_str(&format!("{:013b}", 576));
        bits.push('1');
        bits.push('0');
        bits.push('1');
        bits.push('1'); // sprite_enable = STATIC
        bits.push_str("11111111"); // whatever follows
        let vol = parse_vol(&pack(&bits)).unwrap();
        assert_eq!(vol.bit_depth, None);
        assert_eq!((vol.width, vol.height), (720, 576));
    }

    #[test]
    fn the_studio_vol_carries_depth_and_chroma_directly() {
        // Simple Studio (`video_object_type_indication` 14), 4:2:2 at 10-bit,
        // 1920x1080. The only Part 2 variant that can say any of that — and the
        // frame rate stays unreported even though a `frame_rate_code` is read,
        // because no reachable source states that field's value table.
        let mut bits = String::new();
        bits.push('0'); // random_accessible_vol
        bits.push_str("00001110"); // video_object_type_indication = 14
        bits.push_str("0010"); // video_object_layer_verid
        bits.push_str("00"); // shape = rectangular
        bits.push_str("0000"); // shape extension
        bits.push('1'); // progressive_sequence
        bits.push('0'); // rgb_components
        bits.push_str("10"); // chroma_format = 2 (4:2:2)
        bits.push_str("1010"); // bit_depth = 10
        bits.push('1'); // marker
        bits.push_str(&format!("{:014b}", 1920));
        bits.push('1'); // marker
        bits.push_str(&format!("{:014b}", 1080));
        bits.push('1'); // marker
        bits.push_str("0001"); // aspect_ratio_info
        bits.push_str("0100"); // frame_rate_code = 4
        let vol = parse_vol(&pack(&bits)).unwrap();
        assert_eq!((vol.width, vol.height), (1920, 1080));
        assert_eq!(vol.chroma, Some("4:2:2"));
        assert_eq!(vol.bit_depth, Some(10));
        assert_eq!(vol.fps, None);
    }

    /// MPEG-2 slice start codes (`01`..`AF`) overlap the VOL range
    /// (`20`..`2F`), so MPEG-2 payload mislabelled as Part 2 offers thousands
    /// of VOL candidates and one eventually passes every marker bit. Requiring
    /// dimensions is what stops that turning into a chroma format and a
    /// spec-defined colour description for a stream that is neither.
    #[test]
    fn a_vol_describing_no_picture_is_refused() {
        let mut bits = String::new();
        bits.push('0'); // random_accessible_vol
        bits.push_str("00000001"); // video_object_type_indication = 1
        bits.push('0'); // is_object_layer_identifier
        bits.push_str("0001"); // aspect_ratio_info
        bits.push('0'); // vol_control_parameters absent
        bits.push_str("10"); // shape = binary only: carries no dimensions
        bits.push('1'); // marker
        bits.push_str(&format!("{:016b}", 25));
        bits.push('1'); // marker
        bits.push('0'); // fixed_vop_rate
        let body = pack(&bits);
        // The VOL itself parses — every marker bit is where it should be — and
        // is still refused, because it describes no picture.
        assert!(parse_vol(&body).is_some());
        let mut stream = vec![0x00, 0x00, 0x01, 0x20];
        stream.extend_from_slice(&body);
        assert!(parse_visual(&stream).is_none());
    }

    #[test]
    fn truncation_declines_rather_than_reporting_a_partial_layer() {
        for cut in 19..XVID_HEADERS.len() {
            // Every prefix that cuts into the VOL must decline. A partial parse
            // here would report dimensions assembled from bits that were never
            // read.
            let v = parse_visual(&XVID_HEADERS[..cut]);
            assert!(v.is_none() || v.unwrap().width == 320, "prefix of {cut} bytes");
        }
    }

    #[test]
    fn table_g1_rows_land_where_their_sources_put_them() {
        // Primary-backed rows from 14496-2:2001/Amd.3:2003.
        assert_eq!(profile_level_label(0xF7).as_deref(), Some("Advanced Simple@L3b"));
        assert_eq!(profile_level_label(0x10).as_deref(), Some("Simple Scalable@L0"));
        assert_eq!(profile_level_label(0x08).as_deref(), Some("Simple@L0"));
        // The block the amendment's reserved-range edit pins.
        assert_eq!(profile_level_label(0xF0).as_deref(), Some("Advanced Simple@L0"));
        assert_eq!(profile_level_label(0xF5).as_deref(), Some("Advanced Simple@L5"));
        // Simple's fourth level is L4a; a plain "L4" would be a level that does
        // not exist in this profile.
        assert_eq!(profile_level_label(0x04).as_deref(), Some("Simple@L4a"));
        // Reserved after the amendment, and still reserved.
        assert_eq!(profile_level_label(0xF6), None);
        assert_eq!(profile_level_label(0x00), None);
        assert_eq!(profile_level_label(0x07), None);
    }

    /// Pack a string of '0'/'1' into bytes, MSB first, zero-padding the tail.
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
