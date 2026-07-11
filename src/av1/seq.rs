//! Minimal AV1 sequence-header OBU parser (AV1 spec §5.5).
//!
//! We only need what the report shows for a *raw* elementary stream that has no
//! container metadata: frame dimensions, bit depth, chroma subsampling, and the
//! CICP colour description. We parse just far enough to reach `color_config()`
//! and stop. Never decodes.

use crate::bits::BitReader;
use crate::container::{cicp_matrix, cicp_primaries, cicp_transfer};
use crate::model::ColorInfo;

pub struct SeqInfo {
    pub seq_profile: u8,
    /// First operating point's `seq_tier` (0 = Main tier, 1 = High tier).
    pub seq_tier: u8,
    /// First operating point's `seq_level_idx` (idx = (major−2)×4 + minor).
    pub seq_level_idx: u8,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub chroma: &'static str,
    pub color: ColorInfo,
    /// Whether `color_config()` carried an explicit `color_description` (CICP
    /// triplet). When false, `color`'s CICP fields are the spec's "unspecified"
    /// defaults — the stream declares nothing, which callers recovering colour
    /// from an embedded sequence header must not mistake for a signal (the
    /// HEVC analogue is the SPS VUI's `colour_description_present_flag`).
    pub color_description_present: bool,
    /// Constant frame rate from the sequence header's `timing_info()`, when the
    /// stream carries it with `equal_picture_interval` set. AV1 encoders usually
    /// omit `timing_info` entirely, so this is `None` far more often than for
    /// HEVC — correct-or-`None`, never a guess.
    pub fps: Option<f64>,
}

/// Human label for an AV1 operating point, e.g. `"Main profile, Main tier @ L5.1"`.
///
/// `seq_profile` 0/1/2 = Main/High/Professional — these are *profiles* (they fix
/// bit depth / chroma support), not tiers. AV1 *also* carries a Main/High *tier*
/// via `seq_tier` (only signalled when `seq_level_idx > 7`; Main below that), so
/// the word "profile" is spelled out to keep the two Mains distinct. Levels keep
/// the `X.Y` form AV1 conventionally uses; idx 31 signals no fixed level.
pub fn av1_profile_label(seq_profile: u8, seq_tier: u8, seq_level_idx: u8) -> String {
    let profile = match seq_profile {
        0 => "Main",
        1 => "High",
        2 => "Professional",
        _ => "?",
    };
    let tier = if seq_tier == 1 { "High tier" } else { "Main tier" };
    if seq_level_idx == 31 {
        format!("{profile} profile, {tier}")
    } else {
        let (major, minor) = (2 + (seq_level_idx >> 2), seq_level_idx & 3);
        format!("{profile} profile, {tier} @ L{major}.{minor}")
    }
}

/// Parse a sequence-header OBU payload. Returns `None` on any short read.
pub fn parse_sequence_header(p: &[u8]) -> Option<SeqInfo> {
    let mut r = BitReader::new(p);

    let seq_profile = r.read_bits(3)? as u8;
    let _still_picture = r.read_bit()?;
    let reduced_still_picture_header = r.read_bit()? == 1;

    let mut decoder_model_info_present = false;
    let mut buffer_delay_length = 0u32;
    // First operating point's tier/level (op 0 is the highest); Main tier by
    // default, since seq_tier is only coded when seq_level_idx > 7.
    let mut seq_tier = 0u8;
    let mut seq_level_idx = 0u8;
    let mut fps: Option<f64> = None;

    if reduced_still_picture_header {
        seq_level_idx = r.read_bits(5)? as u8;
    } else {
        let timing_info_present = r.read_bit()? == 1;
        if timing_info_present {
            // timing_info()
            let num_units_in_display_tick = r.read_bits(32)?;
            let time_scale = r.read_bits(32)?;
            let equal_picture_interval = r.read_bit()? == 1;
            if equal_picture_interval {
                // A constant rate only exists when each picture spans a fixed
                // number of display ticks: fps = time_scale / (units·ticks).
                let num_ticks_per_picture = read_uvlc(&mut r)? as u64 + 1;
                let denom = num_units_in_display_tick as u64 * num_ticks_per_picture;
                if denom > 0 && time_scale > 0 {
                    fps = Some(time_scale as f64 / denom as f64).filter(|&f| f > 0.0 && f <= 480.0);
                }
            }
            decoder_model_info_present = r.read_bit()? == 1;
            if decoder_model_info_present {
                // decoder_model_info()
                buffer_delay_length = r.read_bits(5)? + 1;
                r.skip_bits(32)?; // num_units_in_decoding_tick
                r.skip_bits(5)?; // buffer_removal_time_length_minus_1
                r.skip_bits(5)?; // frame_presentation_time_length_minus_1
            }
        }
        let initial_display_delay_present = r.read_bit()? == 1;
        let operating_points_cnt = r.read_bits(5)? + 1;
        for i in 0..operating_points_cnt {
            let _operating_point_idc = r.read_bits(12)?;
            let level_idx = r.read_bits(5)? as u8;
            let tier = if level_idx > 7 { r.read_bits(1)? as u8 } else { 0 };
            if i == 0 {
                seq_level_idx = level_idx;
                seq_tier = tier;
            }
            if decoder_model_info_present {
                let decoder_model_present_for_op = r.read_bit()? == 1;
                if decoder_model_present_for_op {
                    // operating_parameters_info()
                    r.skip_bits(buffer_delay_length)?; // decoder_buffer_delay
                    r.skip_bits(buffer_delay_length)?; // encoder_buffer_delay
                    r.skip_bits(1)?; // low_delay_mode_flag
                }
            }
            if initial_display_delay_present {
                let present_for_op = r.read_bit()? == 1;
                if present_for_op {
                    r.skip_bits(4)?; // initial_display_delay_minus_1
                }
            }
        }
    }

    let frame_width_bits = r.read_bits(4)? + 1;
    let frame_height_bits = r.read_bits(4)? + 1;
    let max_frame_width = r.read_bits(frame_width_bits)? + 1;
    let max_frame_height = r.read_bits(frame_height_bits)? + 1;

    let frame_id_numbers_present = if reduced_still_picture_header {
        false
    } else {
        r.read_bit()? == 1
    };
    if frame_id_numbers_present {
        r.skip_bits(4)?; // delta_frame_id_length_minus_2
        r.skip_bits(3)?; // additional_frame_id_length_minus_1
    }

    r.skip_bits(1)?; // use_128x128_superblock
    r.skip_bits(1)?; // enable_filter_intra
    r.skip_bits(1)?; // enable_intra_edge_filter

    if !reduced_still_picture_header {
        r.skip_bits(1)?; // enable_interintra_compound
        r.skip_bits(1)?; // enable_masked_compound
        r.skip_bits(1)?; // enable_warped_motion
        r.skip_bits(1)?; // enable_dual_filter
        let enable_order_hint = r.read_bit()? == 1;
        if enable_order_hint {
            r.skip_bits(1)?; // enable_jnt_comp
            r.skip_bits(1)?; // enable_ref_frame_mvs
        }
        let seq_choose_screen_content_tools = r.read_bit()? == 1;
        let seq_force_screen_content_tools = if seq_choose_screen_content_tools {
            2 // SELECT_SCREEN_CONTENT_TOOLS
        } else {
            r.read_bits(1)?
        };
        if seq_force_screen_content_tools > 0 {
            let seq_choose_integer_mv = r.read_bit()? == 1;
            if !seq_choose_integer_mv {
                r.skip_bits(1)?; // seq_force_integer_mv
            }
        }
        if enable_order_hint {
            r.skip_bits(3)?; // order_hint_bits_minus_1
        }
    }

    r.skip_bits(1)?; // enable_superres
    r.skip_bits(1)?; // enable_cdef
    r.skip_bits(1)?; // enable_restoration

    let (bit_depth, chroma, color, color_description_present) =
        parse_color_config(&mut r, seq_profile)?;

    Some(SeqInfo {
        seq_profile,
        seq_tier,
        seq_level_idx,
        width: max_frame_width,
        height: max_frame_height,
        bit_depth,
        chroma,
        color,
        color_description_present,
        fps,
    })
}

/// Map AV1 `mono_chrome` + subsampling flags to a chroma-format label.
pub fn av1_chroma_str(mono_chrome: bool, ss_x: u8, ss_y: u8) -> &'static str {
    if mono_chrome {
        "monochrome"
    } else {
        match (ss_x, ss_y) {
            (1, 1) => "4:2:0",
            (1, 0) => "4:2:2",
            (0, 0) => "4:4:4",
            _ => "?",
        }
    }
}

/// color_config() (AV1 spec §5.5.2). Returns (bit_depth, chroma, ColorInfo,
/// color_description_present).
fn parse_color_config(
    r: &mut BitReader,
    seq_profile: u8,
) -> Option<(u8, &'static str, ColorInfo, bool)> {
    let high_bitdepth = r.read_bit()? == 1;
    let bit_depth = if seq_profile == 2 && high_bitdepth {
        let twelve_bit = r.read_bit()? == 1;
        if twelve_bit { 12 } else { 10 }
    } else {
        if high_bitdepth { 10 } else { 8 }
    };

    let mono_chrome = if seq_profile == 1 { false } else { r.read_bit()? == 1 };

    let color_description_present = r.read_bit()? == 1;
    let (mut cp, mut tc, mut mc) = (2u16, 2u16, 2u16); // CICP "unspecified"
    if color_description_present {
        cp = r.read_bits(8)? as u16;
        tc = r.read_bits(8)? as u16;
        mc = r.read_bits(8)? as u16;
    }

    let range_full;
    let (ss_x, ss_y);

    if mono_chrome {
        range_full = r.read_bit()? == 1;
        (ss_x, ss_y) = (1u8, 1u8);
    } else if cp == 1 && tc == 13 && mc == 0 {
        // sRGB special case: full range, 4:4:4.
        range_full = true;
        (ss_x, ss_y) = (0, 0);
    } else {
        range_full = r.read_bit()? == 1;
        (ss_x, ss_y) = match seq_profile {
            0 => (1, 1),
            1 => (0, 0),
            _ => {
                if bit_depth == 12 {
                    let x = r.read_bits(1)? as u8;
                    let y = if x == 1 { r.read_bits(1)? as u8 } else { 0 };
                    (x, y)
                } else {
                    (1, 0)
                }
            }
        };
        if ss_x == 1 && ss_y == 1 {
            r.skip_bits(2)?; // chroma_sample_position
        }
    }

    let chroma = av1_chroma_str(mono_chrome, ss_x, ss_y);

    let color = ColorInfo {
        primaries: cicp_primaries(cp).map(str::to_string),
        transfer: cicp_transfer(tc).map(str::to_string),
        matrix: cicp_matrix(mc).map(str::to_string),
        range: Some(if range_full { "full" } else { "limited" }.to_string()),
    };

    Some((bit_depth, chroma, color, color_description_present))
}

/// AV1 uvlc() — unsigned variable-length code.
fn read_uvlc(r: &mut BitReader) -> Option<u32> {
    let mut leading_zeros = 0u32;
    loop {
        let done = r.read_bit()? == 1;
        if done {
            break;
        }
        leading_zeros += 1;
        if leading_zeros >= 32 {
            return Some(u32::MAX);
        }
    }
    let value = r.read_bits(leading_zeros)?;
    Some(value + (1u32 << leading_zeros) - 1)
}
