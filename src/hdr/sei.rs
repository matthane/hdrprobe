//! HEVC SEI parsing for static HDR + HDR10+.
//!
//! Walks the SEI messages in a prefix/suffix-SEI NAL and pulls out the
//! title-stable pieces we report: mastering-display colour volume (ST.2086,
//! payload 137), content-light level (144), the HLG alternative-transfer
//! override (147), and HDR10+ dynamic metadata (ST.2094-40, carried in a
//! registered ITU-T T.35 message, payload 4).

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

#[derive(Debug, Clone, Default)]
pub struct SeiFindings {
    pub mastering: Option<MasteringDisplay>,
    pub content_light: Option<ContentLight>,
    /// preferred_transfer_characteristics from the HLG alt-transfer SEI.
    pub preferred_transfer: Option<u8>,
    pub hdr10plus: Option<Hdr10PlusInfo>,
}

impl SeiFindings {
    /// Merge another findings set, keeping the first present value of each field
    /// (all are title-stable, so head samples win).
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
    })
}

/// content_light_level_info (MaxCLL/MaxFALL). Shared with the AV1 `HDR_CLL`
/// metadata OBU, which has the same two big-endian u16 layout.
pub(crate) fn parse_content_light(p: &[u8]) -> Option<ContentLight> {
    if p.len() < 4 {
        return None;
    }
    Some(ContentLight {
        max_cll: u16::from_be_bytes([p[0], p[1]]),
        max_fall: u16::from_be_bytes([p[2], p[3]]),
    })
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
}
