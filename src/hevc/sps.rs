//! Minimal HEVC SPS parser — recovers picture dimensions, bit depth, chroma
//! format, and (best-effort) the VUI colour signalling used to classify static
//! HDR when no container box carries it (raw Annex-B, or MKV without a Colour
//! element).

use crate::bits::{ebsp_to_rbsp, BitReader};

/// CICP colour signalling from the SPS VUI (`colour_description` + range).
#[derive(Debug, Clone, Copy)]
pub struct VuiColor {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SpsInfo {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub chroma_format_idc: u8,
    /// `general_profile_idc` from the profile_tier_level (2 = Main 10).
    pub profile_idc: u8,
    /// `general_tier_flag`: true = High tier, false = Main tier.
    pub tier_high: bool,
    /// `general_level_idc` (level × 30, e.g. 153 = 5.1).
    pub level_idc: u8,
    /// VUI colour signalling, when present and parsed successfully.
    pub color: Option<VuiColor>,
    /// Frame rate from the VUI timing info (`vui_time_scale / vui_num_units_in_tick`),
    /// when present. The only in-band frame-rate source for containers without a
    /// timing box (raw Annex-B, TS/M2TS).
    pub frame_rate: Option<f64>,
}

/// What the VUI carries that we surface: colour signalling and frame rate. Both
/// are best-effort — either can be `None` when absent or on a short read.
#[derive(Debug, Clone, Copy, Default)]
struct SpsVui {
    color: Option<VuiColor>,
    frame_rate: Option<f64>,
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

    /// Codec-profile label, e.g. `"Main 10, High tier @ L5.1"`.
    pub fn profile_label(&self) -> String {
        hevc_profile_label(self.profile_idc, self.tier_high, self.level_idc)
    }
}

/// Human label for an HEVC profile_tier_level, shared by the SPS and `hvcC`
/// paths. The profile fixes bit depth/chroma; the *tier* (Main/High) plus level
/// is what bounds bitrate — spelling both out avoids conflating "Main 10" (the
/// profile) with "Main tier".
pub fn hevc_profile_label(profile_idc: u8, tier_high: bool, level_idc: u8) -> String {
    let profile = match profile_idc {
        1 => "Main".to_string(),
        2 => "Main 10".to_string(),
        3 => "Main Still Picture".to_string(),
        4 => "Range Extensions".to_string(),
        n => format!("profile {n}"),
    };
    let tier = if tier_high { "High tier" } else { "Main tier" };
    let (major, minor) = (level_idc / 30, (level_idc % 30) / 3);
    let level = if minor == 0 { major.to_string() } else { format!("{major}.{minor}") };
    format!("{profile}, {tier} @ L{level}")
}

/// Parse an SPS NAL (input includes the 2-byte NAL header).
pub fn parse_sps(nal_with_header: &[u8]) -> Option<SpsInfo> {
    if nal_with_header.len() < 3 {
        return None;
    }
    let rbsp = ebsp_to_rbsp(&nal_with_header[2..]); // skip NAL header
    let mut r = BitReader::new(&rbsp);

    r.skip_bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.read_bits(3)?;
    r.skip_bits(1)?; // sps_temporal_id_nesting_flag

    let (profile_idc, tier_high, level_idc) = parse_profile_tier_level(&mut r, max_sub_layers_minus1)?;

    r.read_ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.read_ue()?;
    if chroma_format_idc == 3 {
        r.skip_bits(1)?; // separate_colour_plane_flag
    }
    let width = r.read_ue()?;
    let height = r.read_ue()?;

    let mut crop_l = 0u32;
    let mut crop_r = 0u32;
    let mut crop_t = 0u32;
    let mut crop_b = 0u32;
    if r.read_bits(1)? == 1 {
        // conformance_window_flag
        crop_l = r.read_ue()?;
        crop_r = r.read_ue()?;
        crop_t = r.read_ue()?;
        crop_b = r.read_ue()?;
    }
    let bit_depth_luma = r.read_ue()? + 8;

    // SubWidthC / SubHeightC for cropping offsets.
    let (sub_w, sub_h) = match chroma_format_idc {
        1 => (2u32, 2u32),
        2 => (2, 1),
        _ => (1, 1),
    };
    let cropped_w = width.saturating_sub((crop_l + crop_r) * sub_w);
    let cropped_h = height.saturating_sub((crop_t + crop_b) * sub_h);

    let mut info = SpsInfo {
        width: if cropped_w > 0 { cropped_w } else { width },
        height: if cropped_h > 0 { cropped_h } else { height },
        bit_depth: bit_depth_luma as u8,
        chroma_format_idc: chroma_format_idc as u8,
        profile_idc,
        tier_high,
        level_idc,
        color: None,
        frame_rate: None,
    };

    // The remainder is best-effort: any failure leaves `color`/`frame_rate` as
    // `None` but keeps the dimensions above (which raw-HEVC demux depends on).
    let vui = parse_vui(&mut r, max_sub_layers_minus1, chroma_format_idc);
    info.color = vui.color;
    info.frame_rate = vui.frame_rate;
    Some(info)
}

/// Continue parsing from just after `bit_depth_luma_minus8` through the VUI,
/// recovering the colour description and the timing info (frame rate). Whatever
/// is read before a short read is retained; the rest stays `None`.
fn parse_vui(r: &mut BitReader, max_sub_layers_minus1: u32, chroma_format_idc: u32) -> SpsVui {
    let mut out = SpsVui::default();
    // Ignore a short read: `out` keeps whatever fields were filled beforehand.
    let _ = parse_vui_inner(r, max_sub_layers_minus1, chroma_format_idc, &mut out);
    out
}

fn parse_vui_inner(
    r: &mut BitReader,
    max_sub_layers_minus1: u32,
    chroma_format_idc: u32,
    out: &mut SpsVui,
) -> Option<()> {
    r.read_ue()?; // bit_depth_chroma_minus8
    let log2_max_poc_lsb = r.read_ue()? + 4; // log2_max_pic_order_cnt_lsb_minus4 + 4

    let sub_layer_ordering = r.read_bits(1)? == 1;
    let start = if sub_layer_ordering { 0 } else { max_sub_layers_minus1 };
    for _ in start..=max_sub_layers_minus1 {
        r.read_ue()?; // max_dec_pic_buffering_minus1
        r.read_ue()?; // num_reorder_pics
        r.read_ue()?; // max_latency_increase_plus1
    }

    r.read_ue()?; // log2_min_luma_coding_block_size_minus3
    r.read_ue()?; // log2_diff_max_min_luma_coding_block_size
    r.read_ue()?; // log2_min_luma_transform_block_size_minus2
    r.read_ue()?; // log2_diff_max_min_luma_transform_block_size
    r.read_ue()?; // max_transform_hierarchy_depth_inter
    r.read_ue()?; // max_transform_hierarchy_depth_intra

    if r.read_bits(1)? == 1 {
        // scaling_list_enabled_flag
        if r.read_bits(1)? == 1 {
            // sps_scaling_list_data_present_flag
            parse_scaling_list_data(r)?;
        }
    }

    r.skip_bits(1)?; // amp_enabled_flag
    r.skip_bits(1)?; // sample_adaptive_offset_enabled_flag

    if r.read_bits(1)? == 1 {
        // pcm_enabled_flag
        r.skip_bits(4)?; // pcm_sample_bit_depth_luma_minus1
        r.skip_bits(4)?; // pcm_sample_bit_depth_chroma_minus1
        r.read_ue()?; // log2_min_pcm_luma_coding_block_size_minus3
        r.read_ue()?; // log2_diff_max_min_pcm_luma_coding_block_size
        r.skip_bits(1)?; // pcm_loop_filter_disabled_flag
    }

    let num_st_rps = r.read_ue()?;
    if num_st_rps > 64 {
        return None; // implausible; bail rather than loop wildly
    }
    let mut num_delta_pocs = vec![0u32; num_st_rps as usize];
    for i in 0..num_st_rps {
        parse_st_ref_pic_set(r, i, num_st_rps, &mut num_delta_pocs)?;
    }

    if r.read_bits(1)? == 1 {
        // long_term_ref_pics_present_flag
        let num_lt = r.read_ue()?;
        for _ in 0..num_lt {
            r.skip_bits(log2_max_poc_lsb)?; // lt_ref_pic_poc_lsb_sps
            r.skip_bits(1)?; // used_by_curr_pic_lt_sps_flag
        }
    }

    r.skip_bits(1)?; // sps_temporal_mvp_enabled_flag
    r.skip_bits(1)?; // strong_intra_smoothing_enabled_flag

    if r.read_bits(1)? != 1 {
        return None; // vui_parameters_present_flag = 0 → no VUI to read
    }
    let _ = chroma_format_idc; // (kept for signature symmetry / future chroma-loc)

    // vui_parameters()
    if r.read_bits(1)? == 1 {
        // aspect_ratio_info_present_flag
        let idc = r.read_bits(8)?;
        if idc == 255 {
            r.skip_bits(16)?; // sar_width
            r.skip_bits(16)?; // sar_height
        }
    }
    if r.read_bits(1)? == 1 {
        // overscan_info_present_flag
        r.skip_bits(1)?; // overscan_appropriate_flag
    }
    if r.read_bits(1)? == 1 {
        // video_signal_type_present_flag
        r.skip_bits(3)?; // video_format
        let full_range = r.read_bits(1)? == 1;
        if r.read_bits(1)? == 1 {
            // colour_description_present_flag
            let primaries = r.read_bits(8)? as u8;
            let transfer = r.read_bits(8)? as u8;
            let matrix = r.read_bits(8)? as u8;
            out.color = Some(VuiColor { primaries, transfer, matrix, full_range });
        }
    }
    if r.read_bits(1)? == 1 {
        // chroma_loc_info_present_flag
        r.read_ue()?; // chroma_sample_loc_type_top_field
        r.read_ue()?; // chroma_sample_loc_type_bottom_field
    }
    r.skip_bits(1)?; // neutral_chroma_indication_flag
    let field_seq = r.read_bits(1)? == 1; // field_seq_flag (fields, not frames)
    r.skip_bits(1)?; // frame_field_info_present_flag
    if r.read_bits(1)? == 1 {
        // default_display_window_flag
        r.read_ue()?; // def_disp_win_left_offset
        r.read_ue()?; // def_disp_win_right_offset
        r.read_ue()?; // def_disp_win_top_offset
        r.read_ue()?; // def_disp_win_bottom_offset
    }
    if r.read_bits(1)? == 1 {
        // vui_timing_info_present_flag
        let num_units_in_tick = r.read_bits(32)?;
        let time_scale = r.read_bits(32)?;
        if num_units_in_tick > 0 && time_scale > 0 {
            let mut fps = time_scale as f64 / num_units_in_tick as f64;
            // When each coded picture is a field, the tick is a field period, so
            // the frame rate is half the tick rate.
            if field_seq {
                fps /= 2.0;
            }
            out.frame_rate = Some(fps);
        }
    }
    Some(())
}

fn parse_scaling_list_data(r: &mut BitReader) -> Option<()> {
    for size_id in 0..4u32 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0u32;
        while matrix_id < 6 {
            if r.read_bits(1)? == 0 {
                // scaling_list_pred_mode_flag == 0
                r.read_ue()?; // scaling_list_pred_matrix_id_delta
            } else {
                let coef_num = core::cmp::min(64u32, 1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.read_se()?; // scaling_list_dc_coef_minus8
                }
                for _ in 0..coef_num {
                    r.read_se()?; // scaling_list_delta_coef
                }
            }
            matrix_id += step;
        }
    }
    Some(())
}

/// Parse `st_ref_pic_set(idx)`, tracking `NumDeltaPocs` so inter-predicted sets
/// consume the correct number of bits.
fn parse_st_ref_pic_set(
    r: &mut BitReader,
    idx: u32,
    num_st_rps: u32,
    num_delta_pocs: &mut [u32],
) -> Option<()> {
    let inter = if idx != 0 { r.read_bits(1)? == 1 } else { false };
    if inter {
        if idx == num_st_rps {
            r.read_ue()?; // delta_idx_minus1 (only in the slice-level extra set)
        }
        r.skip_bits(1)?; // delta_rps_sign
        r.read_ue()?; // abs_delta_rps_minus1
        // delta_idx_minus1 is 0 within the SPS loop → RefRpsIdx = idx - 1.
        let ref_idx = idx.checked_sub(1)? as usize;
        let ref_ndp = *num_delta_pocs.get(ref_idx)?;
        let mut ndp = 0u32;
        for _ in 0..=ref_ndp {
            let used = r.read_bits(1)? == 1;
            let use_delta = if used { true } else { r.read_bits(1)? == 1 };
            if used || use_delta {
                ndp += 1;
            }
        }
        if let Some(slot) = num_delta_pocs.get_mut(idx as usize) {
            *slot = ndp;
        }
    } else {
        let num_neg = r.read_ue()?;
        let num_pos = r.read_ue()?;
        if let Some(slot) = num_delta_pocs.get_mut(idx as usize) {
            *slot = num_neg.saturating_add(num_pos);
        }
        for _ in 0..num_neg {
            r.read_ue()?; // delta_poc_s0_minus1
            r.skip_bits(1)?; // used_by_curr_pic_s0_flag
        }
        for _ in 0..num_pos {
            r.read_ue()?; // delta_poc_s1_minus1
            r.skip_bits(1)?; // used_by_curr_pic_s1_flag
        }
    }
    Some(())
}

/// Parse profile_tier_level, returning the general layer's
/// `(profile_idc, tier_high, level_idc)`; sub-layer entries are skipped.
fn parse_profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: u32) -> Option<(u8, bool, u8)> {
    // general layer: profile_space(2) + tier_flag(1) + profile_idc(5), then 32
    // compat flags + 48 constraint bits, then 8-bit general_level_idc = 96 bits.
    r.skip_bits(2)?; // general_profile_space
    let tier_high = r.read_bits(1)? == 1;
    let profile_idc = r.read_bits(5)? as u8;
    r.skip_bits(32)?; // general_profile_compatibility_flags
    r.skip_bits(48)?; // general_constraint_indicator_flags
    let level_idc = r.read_bits(8)? as u8;

    if max_sub_layers_minus1 == 0 {
        return Some((profile_idc, tier_high, level_idc));
    }

    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.read_bits(1)? == 1;
        level_present[i] = r.read_bits(1)? == 1;
    }
    // reserved_zero_2bits for i in maxSubLayersMinus1..8
    for _ in max_sub_layers_minus1..8 {
        r.skip_bits(2)?;
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.skip_bits(88)?; // sub-layer profile (without level): 96 - 8
        }
        if level_present[i] {
            r.skip_bits(8)?; // sub_layer_level_idc
        }
    }
    Some((profile_idc, tier_high, level_idc))
}

/// Locate the first SPS NAL (type 33) inside an `hvcC` configuration record
/// (MP4 `hvcC` box payload / MKV CodecPrivate). Returns the NAL bytes including
/// the 2-byte NAL header.
pub fn find_sps_in_hvcc(hvcc: &[u8]) -> Option<&[u8]> {
    // Fixed part is 22 bytes; numOfArrays at offset 22.
    if hvcc.len() < 23 {
        return None;
    }
    let num_arrays = hvcc[22] as usize;
    let mut p = 23usize;
    for _ in 0..num_arrays {
        if p + 3 > hvcc.len() {
            return None;
        }
        let nal_type = hvcc[p] & 0x3F;
        let num_nalus = u16::from_be_bytes([hvcc[p + 1], hvcc[p + 2]]) as usize;
        p += 3;
        for _ in 0..num_nalus {
            if p + 2 > hvcc.len() {
                return None;
            }
            let len = u16::from_be_bytes([hvcc[p], hvcc[p + 1]]) as usize;
            p += 2;
            if p + len > hvcc.len() {
                return None;
            }
            if nal_type == 33 {
                return Some(&hvcc[p..p + len]);
            }
            p += len;
        }
    }
    None
}
