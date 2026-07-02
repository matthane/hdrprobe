//! Minimal AVC (H.264) SPS parser — recovers picture dimensions, bit depth,
//! chroma format, the profile/level label, and (best-effort) the VUI colour
//! signalling and frame rate.
//!
//! Parallel to [`crate::hevc::sps`]; it reuses that module's [`VuiColor`] so the
//! shared `container::color_from_vui` plumbing works unchanged. The differences
//! from HEVC are the 1-byte NAL header, the profile_idc-gated high-profile chroma
//! block, macroblock-based dimensions, and the AVC frame-rate convention
//! (`time_scale / (2 * num_units_in_tick)`).

use crate::bits::{ebsp_to_rbsp, BitReader};
use crate::hevc::sps::VuiColor;

#[derive(Debug, Clone, Copy)]
pub struct SpsInfo {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub chroma_format_idc: u8,
    /// `profile_idc` (100 = High, 77 = Main, 66 = Baseline, …).
    pub profile_idc: u8,
    /// `level_idc` (level × 10, e.g. 42 = 4.2).
    pub level_idc: u8,
    /// `constraint_set4_flag` — frame-only (Progressive High when set on High).
    pub constraint_set4: bool,
    /// `constraint_set5_flag` — Constrained High when set on High.
    pub constraint_set5: bool,
    /// VUI colour signalling, when present and parsed successfully.
    pub color: Option<VuiColor>,
    /// Frame rate from the VUI timing info, when present.
    pub frame_rate: Option<f64>,
}

impl SpsInfo {
    pub fn chroma_str(&self) -> &'static str {
        match self.chroma_format_idc {
            0 => "monochrome",
            1 => "4:2:0",
            2 => "4:2:2",
            3 => "4:4:4",
            _ => "?",
        }
    }

    /// Codec-profile label, e.g. `"High @ L4.2"`.
    pub fn profile_label(&self) -> String {
        avc_profile_label(self.profile_idc, self.level_idc, self.constraint_set4, self.constraint_set5)
    }
}

/// Human label for an AVC profile_idc + level_idc. AVC has no Main/High *tier*
/// (that is a Dolby-level concept), so unlike HEVC the label carries only the
/// coding profile and the level. High profile refines to Progressive High
/// (frame-only, `constraint_set4`) or Constrained High (also `constraint_set5`),
/// the three High variants Dolby Vision profile 9 allows.
pub fn avc_profile_label(profile_idc: u8, level_idc: u8, cs4: bool, cs5: bool) -> String {
    let profile = match profile_idc {
        66 => "Baseline".to_string(),
        77 => "Main".to_string(),
        88 => "Extended".to_string(),
        100 => match (cs4, cs5) {
            (true, true) => "Constrained High".to_string(),
            (true, false) => "Progressive High".to_string(),
            _ => "High".to_string(),
        },
        110 => "High 10".to_string(),
        122 => "High 4:2:2".to_string(),
        244 => "High 4:4:4 Predictive".to_string(),
        n => format!("profile {n}"),
    };
    let (major, minor) = (level_idc / 10, level_idc % 10);
    let level = if minor == 0 { major.to_string() } else { format!("{major}.{minor}") };
    format!("{profile} @ L{level}")
}

/// Parse an SPS NAL (input includes the 1-byte NAL header).
pub fn parse_sps(nal_with_header: &[u8]) -> Option<SpsInfo> {
    if nal_with_header.len() < 2 {
        return None;
    }
    let rbsp = ebsp_to_rbsp(&nal_with_header[1..]); // skip the 1-byte NAL header
    let mut r = BitReader::new(&rbsp);

    let profile_idc = r.read_bits(8)? as u8;
    // constraint_set0..5_flags (6) + reserved_zero_2bits (2).
    let constraints = r.read_bits(8)?;
    let constraint_set4 = (constraints >> 3) & 1 == 1;
    let constraint_set5 = (constraints >> 2) & 1 == 1;
    let level_idc = r.read_bits(8)? as u8;
    r.read_ue()?; // seq_parameter_set_id

    // High-profile family carries an explicit chroma/bit-depth block; other
    // profiles imply 4:2:0 8-bit.
    let mut chroma_format_idc = 1u32;
    let mut bit_depth = 8u32;
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.read_ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.read_bit()? == 1;
        }
        bit_depth = r.read_ue()? + 8; // bit_depth_luma_minus8
        r.read_ue()?; // bit_depth_chroma_minus8
        r.skip_bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.read_bit()? == 1 {
            // seq_scaling_matrix_present_flag
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if r.read_bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    skip_scaling_list(&mut r, size)?;
                }
            }
        }
    }

    r.read_ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = r.read_ue()?;
    if pic_order_cnt_type == 0 {
        r.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        r.skip_bits(1)?; // delta_pic_order_always_zero_flag
        r.read_se()?; // offset_for_non_ref_pic
        r.read_se()?; // offset_for_top_to_bottom_field
        let n = r.read_ue()?;
        if n > 256 {
            return None; // implausible; bail rather than loop wildly
        }
        for _ in 0..n {
            r.read_se()?; // offset_for_ref_frame[i]
        }
    }

    r.read_ue()?; // max_num_ref_frames
    r.skip_bits(1)?; // gaps_in_frame_num_value_allowed_flag
    let pic_width_in_mbs = r.read_ue()? + 1;
    let pic_height_in_map_units = r.read_ue()? + 1;
    let frame_mbs_only = r.read_bit()? == 1;
    if !frame_mbs_only {
        r.skip_bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.skip_bits(1)?; // direct_8x8_inference_flag

    let mut crop_l = 0u32;
    let mut crop_r = 0u32;
    let mut crop_t = 0u32;
    let mut crop_b = 0u32;
    if r.read_bit()? == 1 {
        // frame_cropping_flag
        crop_l = r.read_ue()?;
        crop_r = r.read_ue()?;
        crop_t = r.read_ue()?;
        crop_b = r.read_ue()?;
    }

    // Macroblock dimensions, then apply the cropping rectangle. ChromaArrayType
    // is 0 when monochrome or separate colour planes, which changes the crop
    // unit (§7.4.2.1.1).
    let width_mb = pic_width_in_mbs * 16;
    let height_mb = (2 - frame_mbs_only as u32) * pic_height_in_map_units * 16;
    let chroma_array_type = if separate_colour_plane { 0 } else { chroma_format_idc };
    let (sub_w, sub_h) = match chroma_array_type {
        1 => (2u32, 2u32),
        2 => (2, 1),
        3 => (1, 1),
        _ => (1, 1), // monochrome / separate planes
    };
    let crop_unit_x = if chroma_array_type == 0 { 1 } else { sub_w };
    let crop_unit_y = if chroma_array_type == 0 { 2 - frame_mbs_only as u32 } else { sub_h * (2 - frame_mbs_only as u32) };
    let width = width_mb.saturating_sub((crop_l + crop_r) * crop_unit_x);
    let height = height_mb.saturating_sub((crop_t + crop_b) * crop_unit_y);

    let mut info = SpsInfo {
        width: if width > 0 { width } else { width_mb },
        height: if height > 0 { height } else { height_mb },
        bit_depth: bit_depth as u8,
        chroma_format_idc: chroma_format_idc as u8,
        profile_idc,
        level_idc,
        constraint_set4,
        constraint_set5,
        color: None,
        frame_rate: None,
    };

    // The VUI is best-effort: a short read leaves colour/frame_rate as `None`
    // but keeps the dimensions above.
    if r.read_bit() == Some(1) {
        // vui_parameters_present_flag
        parse_vui(&mut r, &mut info);
    }
    Some(info)
}

/// Parse the leading fields of `vui_parameters()` up to and including the timing
/// info, filling `color` / `frame_rate`. Stops before the HRD blocks (not
/// needed). A short read simply leaves later fields unset.
fn parse_vui(r: &mut BitReader, info: &mut SpsInfo) -> Option<()> {
    if r.read_bit()? == 1 {
        // aspect_ratio_info_present_flag
        let idc = r.read_bits(8)?;
        if idc == 255 {
            r.skip_bits(16)?; // sar_width
            r.skip_bits(16)?; // sar_height
        }
    }
    if r.read_bit()? == 1 {
        // overscan_info_present_flag
        r.skip_bits(1)?; // overscan_appropriate_flag
    }
    if r.read_bit()? == 1 {
        // video_signal_type_present_flag
        r.skip_bits(3)?; // video_format
        let full_range = r.read_bit()? == 1;
        if r.read_bit()? == 1 {
            // colour_description_present_flag
            let primaries = r.read_bits(8)? as u8;
            let transfer = r.read_bits(8)? as u8;
            let matrix = r.read_bits(8)? as u8;
            info.color = Some(VuiColor { primaries, transfer, matrix, full_range });
        }
    }
    if r.read_bit()? == 1 {
        // chroma_loc_info_present_flag
        r.read_ue()?; // chroma_sample_loc_type_top_field
        r.read_ue()?; // chroma_sample_loc_type_bottom_field
    }
    if r.read_bit()? == 1 {
        // timing_info_present_flag
        let num_units_in_tick = r.read_bits(32)?;
        let time_scale = r.read_bits(32)?;
        // AVC: a clock tick is num_units_in_tick/time_scale and a frame spans two
        // ticks (fields), so the frame rate is time_scale / (2 * tick).
        if num_units_in_tick > 0 && time_scale > 0 {
            info.frame_rate = Some(time_scale as f64 / (2.0 * num_units_in_tick as f64));
        }
    }
    Some(())
}

fn skip_scaling_list(r: &mut BitReader, size: u32) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = r.read_se()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 { last_scale } else { next_scale };
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_label_high_variants() {
        assert_eq!(avc_profile_label(100, 42, false, false), "High @ L4.2");
        assert_eq!(avc_profile_label(100, 42, true, false), "Progressive High @ L4.2");
        assert_eq!(avc_profile_label(100, 42, true, true), "Constrained High @ L4.2");
        assert_eq!(avc_profile_label(77, 40, false, false), "Main @ L4");
        assert_eq!(avc_profile_label(66, 31, false, false), "Baseline @ L3.1");
    }

    #[test]
    fn parses_real_high_profile_sps() {
        // A real libx264 High-profile SPS (1920×1080, 8-bit 4:2:0) with a Rec.709
        // VUI (colour_primaries/transfer/matrix all 1) — the base-layer signalling
        // of a Dolby Vision profile 9 stream. Includes the 1-byte NAL header 0x67.
        let sps = [
            0x67, 0x64, 0x00, 0x28, 0xac, 0xb2, 0x00, 0xf0, 0x04, 0x4f, 0xcb, 0x80, 0xb5, 0x01,
            0x01, 0x01, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x0c, 0x03, 0xc6, 0x0c,
            0x92,
        ];
        let info = parse_sps(&sps).expect("valid SPS");
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!(info.bit_depth, 8);
        assert_eq!(info.chroma_str(), "4:2:0");
        assert_eq!(info.profile_idc, 100);
        assert_eq!(info.profile_label(), "High @ L4");
        let c = info.color.expect("VUI colour present");
        assert_eq!((c.primaries, c.transfer, c.matrix), (1, 1, 1)); // BT.709
        assert!(!c.full_range); // limited range
    }
}
