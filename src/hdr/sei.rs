//! HEVC SEI parsing for static HDR + HDR10+.
//!
//! Walks the SEI messages in a prefix/suffix-SEI NAL and pulls out the
//! title-stable pieces we report: mastering-display colour volume (ST.2086,
//! payload 137), content-light level (144), the HLG alternative-transfer
//! override (147), and the dynamic-metadata formats riding registered ITU-T
//! T.35 messages (payload 4) — HDR10+ (ST.2094-40) and SL-HDR (ETSI
//! TS 103 433), told apart by their T.35 provider codes.

use hdr10plus::metadata::Hdr10PlusMetadata;

use crate::bits::ebsp_to_rbsp;
use crate::model::{ContentLight, MasteringDisplay};

// SEI payload types of interest.
const SEI_MASTERING_DISPLAY: u32 = 137;
const SEI_CONTENT_LIGHT: u32 = 144;
const SEI_ALT_TRANSFER: u32 = 147;
const SEI_USER_DATA_T35: u32 = 4;

/// HDR10+ presence + the title-stable fields we surface. The per-frame dynamic
/// values (maxscl / histogram / Bézier anchors) are the HDR10+ analogue of DV
/// L1 and are deliberately never reported.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hdr10PlusInfo {
    pub application_version: u8,
    pub num_windows: u8,
    /// ST.2094-40 profile from tone-mapping presence: `b'A'` (histogram only) or
    /// `b'B'` (carries a Bézier tone-mapping curve). 0 when indeterminate.
    pub profile: u8,
    /// targeted_system_display_maximum_luminance (nits); 0 when unsignalled.
    pub target_max_luminance: u32,
}

/// SL-HDR (ETSI TS 103 433) presence + the title-stable header fields of the
/// SL-HDR information SEI. The per-picture reconstruction parameters (the
/// luminance/colour mapping variables) are the SL-HDR analogue of DV L1 and
/// are deliberately never reported.
#[derive(Debug, Clone)]
pub struct SlHdrInfo {
    /// SL-HDR mode from `sl_hdr_mode_value_minus1` + 1: 1 (SDR base),
    /// 2 (PQ base), 3 (HLG base).
    pub mode: u8,
    /// `sl_hdr_spec_major_version_idc` / `sl_hdr_spec_minor_version_idc`.
    pub spec_major: u8,
    pub spec_minor: u8,
    /// `sl_hdr_payload_mode` (a 3-bit field): 0 parameter-based, 1
    /// table-based. `None` when the message carried the cancel flag (the
    /// mode/version half still identifies the format).
    pub payload_mode: Option<u8>,
    /// The target picture block, when present: the presentation the
    /// adaptation metadata is tuned toward. CICP primaries code + max
    /// luminance in cd/m² (corpus-verified title-stable: identical on every
    /// frame of the SL-HDR2 feature clip).
    pub target_primaries: Option<u8>,
    pub target_max_nits: Option<u16>,
    /// The source mastering display block (`src_mdcv_*`), when present:
    /// ST.2086-shaped chromaticities (role-canonicalized like every other
    /// mastering read), max in whole cd/m², min in 0.0001 cd/m² units. Also
    /// corpus-verified title-stable.
    pub source_mastering: Option<crate::model::MasteringDisplay>,
}

/// HDR Vivid (CUVA, T/UWA 005) presence + title-stable facts. The per-frame
/// tone-mapping payload (maxrgb statistics, base-curve and spline parameters)
/// is the HDR Vivid analogue of DV L1 and is never reported — but each
/// parameter set's `targeted_system_display_maximum_luminance` is an authored
/// display anchor (corpus-verified constant across every frame of three
/// independent 1500-frame samples), so those are collected as a distinct set,
/// the HDR Vivid analogue of the DV trim-target set.
#[derive(Debug, Clone)]
pub struct HdrVividInfo {
    /// CUVA metadata version from the T.35 provider-oriented code
    /// (0x0005 → 1 … 0x0008 → 4, per T/UWA 005.2-1 Table 5).
    pub version: u8,
    /// `system_start_code`, the dynamic-metadata data-set type (0x01..=0x07
    /// admitted; every known stream signals 0x01).
    pub system_start_code: u8,
    /// Distinct `targeted_system_display_maximum_luminance` codes (12-bit PQ)
    /// across the tone-mapping parameter sets, in first-seen order. Bounded by
    /// [`HDR_VIVID_MAX_TARGETS`]; a stream churning past the cap is per-frame
    /// data, not a target list, and stops accumulating.
    pub target_pq: Vec<u16>,
}

/// Cap on distinct HDR Vivid targets collected. Real titles carry one or two
/// (an HDR anchor and an SDR one); anything unbounded would be per-frame data.
pub const HDR_VIVID_MAX_TARGETS: usize = 8;

impl HdrVividInfo {
    /// Fold another frame's finding into this one: version/start-code stay
    /// first-wins, the target set unions (order-insensitive, so the batch
    /// aggregation order rules don't apply pressure here).
    pub fn absorb(&mut self, other: &HdrVividInfo) {
        for t in &other.target_pq {
            if self.target_pq.len() >= HDR_VIVID_MAX_TARGETS {
                break;
            }
            if !self.target_pq.contains(t) {
                self.target_pq.push(*t);
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SeiFindings {
    pub mastering: Option<MasteringDisplay>,
    pub content_light: Option<ContentLight>,
    /// preferred_transfer_characteristics from the HLG alt-transfer SEI.
    pub preferred_transfer: Option<u8>,
    pub hdr10plus: Option<Hdr10PlusInfo>,
    pub sl_hdr: Option<SlHdrInfo>,
    pub hdr_vivid: Option<HdrVividInfo>,
}

impl SeiFindings {
    /// Merge another findings set, keeping the first present value of each
    /// field (all are title-stable, so head samples win) — except the HDR
    /// Vivid target set, which unions across frames like the DV trim-target
    /// set (order-insensitive, so batch order can't change it).
    pub fn merge(&mut self, other: &SeiFindings) {
        if self.mastering.is_none() {
            self.mastering = other.mastering.clone();
        }
        if self.content_light.is_none() {
            self.content_light = other.content_light;
        }
        if self.preferred_transfer.is_none() {
            self.preferred_transfer = other.preferred_transfer;
        }
        if self.hdr10plus.is_none() {
            self.hdr10plus = other.hdr10plus;
        }
        if self.sl_hdr.is_none() {
            self.sl_hdr = other.sl_hdr.clone();
        }
        match (&mut self.hdr_vivid, &other.hdr_vivid) {
            (Some(mine), Some(theirs)) => mine.absorb(theirs),
            (None, Some(theirs)) => self.hdr_vivid = Some(theirs.clone()),
            _ => {}
        }
    }
}

/// Parse one HEVC SEI NAL (input includes the 2-byte NAL header) into findings.
pub fn parse_sei_nal(nal_with_header: &[u8]) -> SeiFindings {
    parse_sei_nal_hdr(nal_with_header, 2)
}

/// Parse one AVC SEI NAL (input includes the 1-byte NAL header) into findings.
/// The SEI RBSP (payloadType/payloadSize message loop) is identical across AVC
/// and HEVC; only the NAL header length differs.
pub fn parse_sei_nal_avc(nal_with_header: &[u8]) -> SeiFindings {
    parse_sei_nal_hdr(nal_with_header, 1)
}

/// Shared SEI-RBSP parser; `hdr_len` is the codec's NAL header size in bytes.
fn parse_sei_nal_hdr(nal_with_header: &[u8], hdr_len: usize) -> SeiFindings {
    let mut out = SeiFindings::default();
    if nal_with_header.len() < hdr_len + 1 {
        return out;
    }
    let rbsp = ebsp_to_rbsp(&nal_with_header[hdr_len..]);
    let mut p = 0usize;
    let n = rbsp.len();

    // Iterate SEI messages until the rbsp trailing bits (0x80) / exhaustion.
    while p < n && rbsp[p] != 0x80 {
        let Some((payload_type, np)) = read_ff_sum(&rbsp, p) else { break };
        let Some((payload_size, np)) = read_ff_sum(&rbsp, np) else { break };
        let start = np;
        let end = start + payload_size as usize;
        if end > n {
            break;
        }
        handle_payload(payload_type, &rbsp[start..end], &mut out);
        p = end;
    }
    out
}

fn handle_payload(payload_type: u32, payload: &[u8], out: &mut SeiFindings) {
    match payload_type {
        SEI_MASTERING_DISPLAY => {
            if let Some(m) = parse_mastering(payload) {
                out.mastering = Some(m);
            }
        }
        SEI_CONTENT_LIGHT => {
            if let Some(cl) = parse_content_light(payload) {
                out.content_light = Some(cl);
            }
        }
        SEI_ALT_TRANSFER => {
            if let Some(&pref) = payload.first() {
                out.preferred_transfer = Some(pref);
            }
        }
        SEI_USER_DATA_T35 => {
            if let Some(info) = parse_hdr10plus(payload) {
                out.hdr10plus = Some(info);
            } else if let Some(info) = parse_sl_hdr(payload) {
                out.sl_hdr = Some(info);
            } else if let Some(info) = parse_hdr_vivid(payload) {
                match &mut out.hdr_vivid {
                    Some(mine) => mine.absorb(&info),
                    slot => *slot = Some(info),
                }
            }
        }
        _ => {}
    }
}

/// mastering_display_colour_volume: G/B/R primaries + white point (x,y u16
/// each, 0.00002 units), then max (32b) and min (32b) luminance in units of
/// 0.0001 cd/m². The ISOBMFF `mdcv` box shares this layout and scaling. The
/// AV1 `HDR_MDCV` OBU matches in *shape only* — its primary order and
/// fixed-point scales differ, so it has its own parser below.
pub(crate) fn parse_mastering(p: &[u8]) -> Option<MasteringDisplay> {
    if p.len() < 24 {
        return None;
    }
    let xy = |i: usize| {
        (
            u16::from_be_bytes([p[i], p[i + 1]]) as f64 / 50000.0,
            u16::from_be_bytes([p[i + 2], p[i + 3]]) as f64 / 50000.0,
        )
    };
    // ST.2086 orders the primaries green, blue, red.
    let (g, b, r, wp) = (xy(0), xy(4), xy(8), xy(12));
    let max_lum = u32::from_be_bytes([p[16], p[17], p[18], p[19]]);
    let min_lum = u32::from_be_bytes([p[20], p[21], p[22], p[23]]);
    Some(MasteringDisplay {
        max_luminance: max_lum as f64 / 10000.0,
        min_luminance: min_lum as f64 / 10000.0,
        primaries: crate::hdr::primaries_label(r, g, b, wp).map(str::to_string),
        primaries_level: None,
    })
}

/// AV1 `metadata_hdr_mdcv`: the same 24-byte shape as ST.2086 but different
/// semantics — primaries in R/G/B order as 0.16 fixed-point chromaticities,
/// `luminance_max` 24.8 and `luminance_min` 18.14 fixed point. Luminance is
/// rounded to the 0.0001 cd/m² display quantum so a fixed-point min of
/// 82/16384 prints as the 0.005 it encodes, matching the same title's
/// container-carried value.
pub(crate) fn parse_mastering_av1(p: &[u8]) -> Option<MasteringDisplay> {
    if p.len() < 24 {
        return None;
    }
    let xy = |i: usize| {
        (
            u16::from_be_bytes([p[i], p[i + 1]]) as f64 / 65536.0,
            u16::from_be_bytes([p[i + 2], p[i + 3]]) as f64 / 65536.0,
        )
    };
    let (r, g, b, wp) = (xy(0), xy(4), xy(8), xy(12));
    let max_lum = u32::from_be_bytes([p[16], p[17], p[18], p[19]]) as f64 / 256.0;
    let min_lum = u32::from_be_bytes([p[20], p[21], p[22], p[23]]) as f64 / 16384.0;
    let quant = |v: f64| (v * 10000.0).round() / 10000.0;
    Some(MasteringDisplay {
        max_luminance: quant(max_lum),
        min_luminance: quant(min_lum),
        primaries: crate::hdr::primaries_label(r, g, b, wp).map(str::to_string),
        primaries_level: None,
    })
}

/// content_light_level_info (MaxCLL/MaxFALL). Shared with the AV1 `HDR_CLL`
/// metadata OBU, which has the same two big-endian u16 layout.
pub(crate) fn parse_content_light(p: &[u8]) -> Option<ContentLight> {
    if p.len() < 4 {
        return None;
    }
    Some(ContentLight::new(u16::from_be_bytes([p[0], p[1]]), u16::from_be_bytes([p[2], p[3]])))
}

/// HDR10+ (ST.2094-40) rides in a registered ITU-T T.35 message: country code
/// 0xB5, provider 0x003C, oriented code 0x0001. Gate on that signature before
/// handing the payload to the `hdr10plus` crate.
pub(crate) fn parse_hdr10plus(p: &[u8]) -> Option<Hdr10PlusInfo> {
    if p.len() < 5 || p[0] != 0xB5 || p[1] != 0x00 || p[2] != 0x3C || p[3] != 0x00 || p[4] != 0x01 {
        return None;
    }
    let meta = crate::dv::rpu::guard(|| Hdr10PlusMetadata::parse(p).ok())?;
    // The crate labels the profile "A"/"B"/"N/A"; keep only a signalled A or B.
    let profile = meta.profile.bytes().next().filter(|b| matches!(b, b'A' | b'B')).unwrap_or(0);
    Some(Hdr10PlusInfo {
        application_version: meta.application_version,
        num_windows: meta.num_windows,
        profile,
        target_max_luminance: meta.targeted_system_display_maximum_luminance,
    })
}

/// SL-HDR (ETSI TS 103 433) rides its own registered ITU-T T.35 message:
/// country code 0xB5, provider 0x003A (ETSI), oriented-code message idc 0x00
/// (the SL-HDR information SEI). The header then packs
/// `sl_hdr_mode_value_minus1`(4) + `sl_hdr_spec_major_version_idc`(4),
/// `sl_hdr_spec_minor_version_idc`(7) + `sl_hdr_cancel_flag`(1), and — when
/// not cancelled — five presence/persistence flag bits then
/// `sl_hdr_payload_mode`(3) (TS 103 433-1 Annex A.2, cross-checked against
/// MediaInfoLib's parser of the same annex and corpus-verified byte-for-byte
/// against MediaInfo on the SL-HDR2 feature clip). The optional info blocks
/// that follow in declaration order — coded picture (5 bytes, not reported),
/// target picture (5), source MDCV (20) — are title-stable (verified
/// identical across every frame of the corpus feature) and are read;
/// everything after them is the per-picture reconstruction payload (matrix
/// coefficients onward), which is never reported.
pub(crate) fn parse_sl_hdr(p: &[u8]) -> Option<SlHdrInfo> {
    if p.len() < 6 || p[0] != 0xB5 || p[1] != 0x00 || p[2] != 0x3A || p[3] != 0x00 {
        return None;
    }
    let mode = (p[4] >> 4) + 1;
    // Modes beyond 3 are reserved: an unknown ETSI message, not an SL-HDR
    // variant we could name — never guess.
    if mode > 3 {
        return None;
    }
    let cancel = p[5] & 1 == 1;
    let mut info = SlHdrInfo {
        mode,
        spec_major: p[4] & 0x0F,
        spec_minor: p[5] >> 1,
        payload_mode: if cancel { None } else { p.get(6).map(|b| b & 0x07) },
        target_primaries: None,
        target_max_nits: None,
        source_mastering: None,
    };
    if let Some(&flags) = p.get(6).filter(|_| !cancel) {
        let mut q = 7usize;
        if flags & 0x40 != 0 {
            q += 5; // coded_picture_info: the coded stream's own CICP, not reported
        }
        if flags & 0x20 != 0 {
            if let Some(b) = p.get(q..q + 5) {
                info.target_primaries = Some(b[0]);
                info.target_max_nits = Some(u16::from_be_bytes([b[1], b[2]]));
            }
            q += 5;
        }
        if flags & 0x10 != 0 {
            info.source_mastering = p.get(q..q + 20).and_then(parse_src_mdcv);
        }
    }
    Some(info)
}

/// The SL-HDR `src_mdcv_*` block: ST.2086's G/B/R + white-point chromaticities
/// in 0.00002 units, but a compact u16 luminance pair — max in whole cd/m²,
/// min in 0.0001 cd/m² units (the asymmetric scaling MediaInfoLib normalizes
/// the same way). A zero max luminance marks the block unfilled — `None`.
fn parse_src_mdcv(b: &[u8]) -> Option<MasteringDisplay> {
    let xy = |i: usize| {
        (
            u16::from_be_bytes([b[i], b[i + 1]]) as f64 / 50000.0,
            u16::from_be_bytes([b[i + 2], b[i + 3]]) as f64 / 50000.0,
        )
    };
    let (g, bl, r, wp) = (xy(0), xy(4), xy(8), xy(12));
    let max = u16::from_be_bytes([b[16], b[17]]);
    if max == 0 {
        return None;
    }
    Some(MasteringDisplay {
        max_luminance: max as f64,
        min_luminance: u16::from_be_bytes([b[18], b[19]]) as f64 / 10000.0,
        primaries: crate::hdr::primaries_label(r, g, bl, wp).map(str::to_string),
        primaries_level: None,
    })
}

/// HDR Vivid (CUVA, T/UWA 005) rides a China-registered T.35 message: country
/// code 0x26, provider 0x0004, then a **2-byte** provider-oriented code that
/// is the CUVA version signal (0x0005 → v1.0 … 0x0008 → v4.0, T/UWA 005.2-1
/// Table 5 — unlike ETSI's 1-byte oriented code), then `system_start_code`
/// (the data-set type, 0x01..=0x07 valid, 0x01 everywhere in practice — the
/// same gate ffmpeg's parser applies). The same byte layout rides the AV1
/// T.35 metadata OBU. The tone-mapping payload after it is per-frame and
/// never reported, except each parameter set's leading
/// `targeted_system_display_maximum_luminance` (an authored display anchor;
/// see `HdrVividInfo`) — reaching the second set means stepping over the
/// first set's body, so the base-curve/spline field widths below follow
/// T/UWA 005.1 Table 11 exactly as ffmpeg's `dynamic_hdr_vivid.c` reads them.
pub(crate) fn parse_hdr_vivid(p: &[u8]) -> Option<HdrVividInfo> {
    if p.len() < 6 || p[0] != 0x26 || p[1] != 0x00 || p[2] != 0x04 || p[3] != 0x00 {
        return None;
    }
    // Oriented codes outside the published version table are an unknown CUVA
    // message, not an HDR Vivid version we could name — never guess.
    let version = match p[4] {
        0x05..=0x08 => p[4] - 0x04,
        _ => return None,
    };
    let system_start_code = p[5];
    if !(0x01..=0x07).contains(&system_start_code) {
        return None;
    }
    let mut info = HdrVividInfo { version, system_start_code, target_pq: Vec::new() };
    // num_windows == 1 by construction for every valid start code (Table 11).
    // A target is pushed the moment its own bits parse; truncation later in
    // the walk just ends collection — located values are complete data.
    let r = &mut crate::bits::BitReader::new(&p[6..]);
    let walk = |r: &mut crate::bits::BitReader, info: &mut HdrVividInfo| -> Option<()> {
        r.skip_bits(48)?; // min/avg/variance/max maxrgb_pq, u12 each
        if r.read_bit()? == 1 {
            let sets = r.read_bit()? + 1;
            for _ in 0..sets {
                let target = r.read_bits(12)? as u16;
                if !info.target_pq.contains(&target) {
                    info.target_pq.push(target);
                }
                if r.read_bit()? == 1 {
                    // base curve: m_p(14) m_m(6) m_a(10) m_b(10) m_n(6)
                    // k1(2) k2(2) k3(4) delta_mode(3) delta(7)
                    r.skip_bits(64)?;
                }
                if r.read_bit()? == 1 {
                    let splines = r.read_bit()? + 1;
                    for _ in 0..splines {
                        let th_mode = r.read_bits(2)?;
                        if th_mode == 0 || th_mode == 2 {
                            r.skip_bits(8)?; // th_enable_mb
                        }
                        r.skip_bits(40)?; // th(12) delta1(10) delta2(10) strength(8)
                    }
                }
            }
        }
        Some(())
    };
    let _ = walk(r, &mut info);
    Some(info)
}

/// Read an SEI `ff_byte`-summed value (payloadType / payloadSize encoding).
fn read_ff_sum(d: &[u8], mut p: usize) -> Option<(u32, usize)> {
    let mut val = 0u32;
    loop {
        let b = *d.get(p)?;
        p += 1;
        val += b as u32;
        if b != 0xFF {
            return Some((val, p));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Prefix-SEI NAL header (type 39): (39<<1)=0x4E, then 0x01.
    fn wrap(msgs: &[u8]) -> Vec<u8> {
        let mut v = vec![0x4E, 0x01];
        v.extend_from_slice(msgs);
        v.push(0x80); // rbsp trailing
        v
    }

    #[test]
    fn mastering_and_content_light_in_one_nal() {
        // 137 (mastering, 24B): BT.2020 primaries in ST.2086's G,B,R order +
        // D65 white (0.00002 units) + max=10_000_000 + min=50.
        let mut mdcv = vec![0x89, 0x18];
        for v in [8500u16, 39850, 6550, 2300, 35400, 14600, 15635, 16450] {
            mdcv.extend_from_slice(&v.to_be_bytes());
        }
        mdcv.extend_from_slice(&10_000_000u32.to_be_bytes()); // 1000 cd/m²
        mdcv.extend_from_slice(&50u32.to_be_bytes()); // 0.005 cd/m²
        // 144 (content light, 4B): MaxCLL 3597, MaxFALL 505.
        let mut cll = vec![0x90, 0x04];
        cll.extend_from_slice(&3597u16.to_be_bytes());
        cll.extend_from_slice(&505u16.to_be_bytes());

        let mut msgs = mdcv;
        msgs.extend_from_slice(&cll);
        let f = parse_sei_nal(&wrap(&msgs));

        let m = f.mastering.expect("mastering");
        assert_eq!(m.max_luminance, 1000.0);
        assert_eq!(m.min_luminance, 0.005);
        assert_eq!(m.primaries.as_deref(), Some("BT.2020"));
        let cl = f.content_light.expect("content light");
        assert_eq!(cl.max_cll, 3597);
        assert_eq!(cl.max_fall, 505);
    }

    #[test]
    fn alt_transfer_hlg() {
        // 147 (alt transfer, 1B): preferred_transfer_characteristics = 18 (HLG).
        let f = parse_sei_nal(&wrap(&[0x93, 0x01, 18]));
        assert_eq!(f.preferred_transfer, Some(18));
    }

    #[test]
    fn hdr10plus_signature_gate_rejects_other_t35() {
        // type 4 T.35 with a non-HDR10+ country/provider signature.
        let payload = [0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(parse_hdr10plus(&payload).is_none());
        // A Dolby-ish T.35 (country 0xB5 but wrong provider) is also rejected.
        let dolby = [0xB5, 0x00, 0x3B, 0x00, 0x00];
        assert!(parse_hdr10plus(&dolby).is_none());
    }

    #[test]
    fn sl_hdr2_header_parses_from_corpus_bytes() {
        // The corpus sl-hdr2.mkv's per-frame T.35 head verbatim: ETSI provider,
        // message idc 0, mode_minus1=1 / major=1 (0x11), minor=0 / no cancel
        // (0x00), flags + payload_mode=0 (0x30: target + src MDCV present),
        // then the target picture block (CICP 9, 100 cd/m², min 0) and the
        // src_mdcv block (BT.2020/D65 in ST.2086's G,B,R order, max 1000,
        // min 0). MediaInfo reads the same head as SL-HDR2 1.0 parameter-based;
        // the blocks were verified identical across all 719 frames.
        let mut p = vec![0xB5, 0x00, 0x3A, 0x00, 0x11, 0x00, 0x30];
        p.push(9); // target_picture_primaries
        p.extend_from_slice(&100u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        for v in [8500u16, 39850, 6550, 2300, 35400, 14600, 15635, 16450, 1000, 0] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        let sl = parse_sl_hdr(&p).expect("SL-HDR2 header");
        assert_eq!(sl.mode, 2);
        assert_eq!((sl.spec_major, sl.spec_minor), (1, 0));
        assert_eq!(sl.payload_mode, Some(0));
        assert_eq!(sl.target_primaries, Some(9));
        assert_eq!(sl.target_max_nits, Some(100));
        let m = sl.source_mastering.expect("src mdcv");
        assert_eq!(m.max_luminance, 1000.0);
        assert_eq!(m.min_luminance, 0.0);
        assert_eq!(m.primaries.as_deref(), Some("BT.2020"));
        // Riding a full SEI NAL, it lands on the findings without disturbing
        // the HDR10+ slot.
        let mut msg = vec![0x04, p.len() as u8];
        msg.extend_from_slice(&p);
        let f = parse_sei_nal(&wrap(&msg));
        assert_eq!(f.sl_hdr.expect("finding").mode, 2);
        assert!(f.hdr10plus.is_none());
    }

    #[test]
    fn sl_hdr_truncated_blocks_keep_the_header() {
        // A payload cut inside the target block still identifies the format;
        // the optional facts just stay unset — never a partial read.
        let p = [0xB5, 0x00, 0x3A, 0x00, 0x11, 0x00, 0x30, 0x09];
        let sl = parse_sl_hdr(&p).expect("header survives");
        assert_eq!(sl.mode, 2);
        assert_eq!(sl.target_primaries, None);
        assert_eq!(sl.target_max_nits, None);
        assert!(sl.source_mastering.is_none());
        // A coded-picture block shifts the later blocks by its 5 bytes.
        let mut p = vec![0xB5, 0x00, 0x3A, 0x00, 0x11, 0x00, 0x70];
        p.extend_from_slice(&[2, 0x01, 0xF4, 0, 0]); // coded picture, skipped
        p.push(9);
        p.extend_from_slice(&400u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        let sl = parse_sl_hdr(&p).expect("shifted target");
        assert_eq!(sl.target_primaries, Some(9));
        assert_eq!(sl.target_max_nits, Some(400));
    }

    #[test]
    fn sl_hdr_gate_rejects_foreign_and_reserved_t35() {
        // HDR10+'s provider (0x003C) must not read as SL-HDR.
        assert!(parse_sl_hdr(&[0xB5, 0x00, 0x3C, 0x00, 0x01, 0x00]).is_none());
        // A non-zero oriented-code message idc is a different ETSI message.
        assert!(parse_sl_hdr(&[0xB5, 0x00, 0x3A, 0x01, 0x11, 0x00]).is_none());
        // A reserved mode value (mode_value_minus1 = 3) is never guessed.
        assert!(parse_sl_hdr(&[0xB5, 0x00, 0x3A, 0x00, 0x31, 0x00]).is_none());
        // A cancel message still identifies the format, minus the payload mode.
        let sl = parse_sl_hdr(&[0xB5, 0x00, 0x3A, 0x00, 0x11, 0x01, 0x30]).expect("cancel");
        assert_eq!(sl.mode, 2);
        assert_eq!(sl.payload_mode, None);
        // The bit above the 3-bit payload-mode field is the extension flag —
        // it must not leak into the mode value (0x38 = extension set, mode 0).
        let sl = parse_sl_hdr(&[0xB5, 0x00, 0x3A, 0x00, 0x11, 0x00, 0x38]).expect("extension");
        assert_eq!(sl.payload_mode, Some(0));
    }

    #[test]
    fn hdr_vivid_targets_walk_both_parameter_sets() {
        // A two-set payload shaped like the corpus B.1-03 frames: stats,
        // mode flag 1, two sets — set 0 target 2770 with a base curve (64
        // bits stepped over), set 1 target 2080 with neither base nor
        // spline. Reaching target 1 proves the set-0 body walk is exact.
        let mut bits = String::new();
        bits.push_str(&"0".repeat(48)); // maxrgb stats
        bits.push('1'); // tone_mapping_mode_flag
        bits.push('1'); // param_enable_num -> 2 sets
        bits.push_str(&format!("{:012b}", 2770));
        bits.push('1'); // base_enable
        bits.push_str(&"0".repeat(64)); // base curve params
        bits.push('0'); // no 3-spline
        bits.push_str(&format!("{:012b}", 2080));
        bits.push('0'); // no base
        bits.push('0'); // no 3-spline
        let mut p = vec![0x26, 0x00, 0x04, 0x00, 0x05, 0x01];
        for chunk in bits.as_bytes().chunks(8) {
            let byte = chunk.iter().fold(0u8, |acc, &b| (acc << 1) | (b - b'0'));
            p.push(byte << (8 - chunk.len()));
        }
        let hv = parse_hdr_vivid(&p).expect("two-set payload");
        assert_eq!(hv.target_pq, vec![2770, 2080]);
        // Truncation right after the first target keeps it — located values
        // are complete data; the walk just stops. 62 bits of walk (stats +
        // flags + target) land in 8 payload bytes after the 6-byte head.
        let hv = parse_hdr_vivid(&p[..14]).expect("truncated payload");
        assert_eq!(hv.target_pq, vec![2770]);
    }

    #[test]
    fn hdr_vivid_header_parses_and_gates() {
        // The CUVA v1.0 signature: country 0x26, provider 0x0004, oriented
        // code 0x0005, system_start_code 0x01.
        let hv = parse_hdr_vivid(&[0x26, 0x00, 0x04, 0x00, 0x05, 0x01]).expect("HDR Vivid");
        assert_eq!(hv.version, 1);
        assert_eq!(hv.system_start_code, 1);
        // Oriented code 0x0008 is the published v4.0.
        let hv = parse_hdr_vivid(&[0x26, 0x00, 0x04, 0x00, 0x08, 0x01]).expect("v4");
        assert_eq!(hv.version, 4);
        // An unpublished oriented code, a reserved start code (0x00 / >0x07),
        // and a foreign provider are all rejected, never guessed.
        assert!(parse_hdr_vivid(&[0x26, 0x00, 0x04, 0x00, 0x09, 0x01]).is_none());
        assert!(parse_hdr_vivid(&[0x26, 0x00, 0x04, 0x00, 0x05, 0x00]).is_none());
        assert!(parse_hdr_vivid(&[0x26, 0x00, 0x04, 0x00, 0x05, 0x08]).is_none());
        assert!(parse_hdr_vivid(&[0x26, 0x00, 0x05, 0x00, 0x05, 0x01]).is_none());
        assert!(parse_hdr_vivid(&[0xB5, 0x00, 0x04, 0x00, 0x05, 0x01]).is_none());
    }
}
