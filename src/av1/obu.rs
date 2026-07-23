//! AV1 Open Bitstream Unit (OBU) walking, for the metadata we report.
//!
//! We never decode. We walk the low-overhead OBU stream inside each sampled
//! access unit and pull metadata OBUs (`OBU_METADATA`, type 5) apart:
//!   - ITU-T T.35 (metadata_type 4): Dolby Vision RPU (provider `0x003B`) →
//!     libdovi, or HDR10+ (provider `0x003C`) → the `hdr10plus` crate;
//!   - HDR content-light (metadata_type 1), byte-identical to the HEVC SEI
//!     equivalent, and mastering-display (type 2), which shares the SEI's
//!     24-byte shape but carries AV1's own primary order and fixed-point
//!     scales (`sei::parse_mastering_av1`).

use dolby_vision::rpu::dovi_rpu::DoviRpu;

use crate::dv::rpu::parse_av1_rpu;
use crate::hdr::sei::{self, SeiFindings};

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_METADATA: u8 = 5;

const METADATA_TYPE_HDR_CLL: u64 = 1;
const METADATA_TYPE_HDR_MDCV: u64 = 2;
const METADATA_TYPE_ITUT_T35: u64 = 4;

// ITU-T T.35 terminal provider codes (after country code 0xB5).
const PROVIDER_DOLBY: u16 = 0x003B;
const PROVIDER_HDR10PLUS: u16 = 0x003C;

/// RPUs plus static-HDR/HDR10+ findings from one access unit's OBU stream.
#[derive(Default)]
pub struct Av1Scan {
    pub rpus: Vec<DoviRpu>,
    pub sei: SeiFindings,
}

/// One OBU: its type, its payload slice, and the offset of its first byte
/// (the OBU header) within the parent buffer.
pub struct Obu<'a> {
    pub obu_type: u8,
    pub payload: &'a [u8],
    pub start: usize,
}

/// Iterate the OBUs in a low-overhead stream (each carrying `obu_has_size_field`,
/// as produced by ISOBMFF/MKV samples, IVF frames and raw `.obu`). Stops on the
/// first framing error (lost sync).
pub fn obus(data: &[u8]) -> impl Iterator<Item = Obu<'_>> {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        if i >= data.len() {
            return None;
        }
        let header = data[i];
        if header & 0x80 != 0 {
            return None; // forbidden_bit set → lost sync
        }
        let obu_type = (header >> 3) & 0x0F;
        let ext_flag = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;

        let mut pos = i + 1;
        if ext_flag {
            pos += 1; // temporal_id / spatial_id byte
        }
        let payload_len = if has_size {
            read_leb128(data, &mut pos)? as usize
        } else {
            data.len().saturating_sub(pos) // valid only as the final OBU
        };
        let start = i;
        let payload_start = pos;
        let end = payload_start + payload_len;
        if end > data.len() {
            return None;
        }
        i = end;
        Some(Obu { obu_type, payload: &data[payload_start..end], start })
    })
}

/// Walk the OBUs in `data` (a single AU's worth) and extract DV RPUs +
/// static-HDR findings.
pub fn scan_obus(data: &[u8]) -> Av1Scan {
    let mut out = Av1Scan::default();
    for obu in obus(data) {
        if obu.obu_type == OBU_METADATA {
            handle_metadata(obu.payload, &mut out);
        }
    }
    out
}

fn handle_metadata(p: &[u8], out: &mut Av1Scan) {
    let mut pos = 0usize;
    let Some(mtype) = read_leb128(p, &mut pos) else { return };
    let body = &p[pos..];
    match mtype {
        METADATA_TYPE_ITUT_T35 => handle_t35(body, out),
        METADATA_TYPE_HDR_CLL => {
            if let Some(cl) = sei::parse_content_light(body) {
                out.sei.content_light.get_or_insert(cl);
            }
        }
        METADATA_TYPE_HDR_MDCV => {
            if let Some(m) = sei::parse_mastering_av1(body) {
                out.sei.mastering.get_or_insert(m);
            }
        }
        _ => {}
    }
}

/// `body` starts at `itu_t_t35_country_code`. Route by terminal provider code.
fn handle_t35(body: &[u8], out: &mut Av1Scan) {
    if body.len() < 3 {
        return;
    }
    // HDR Vivid rides the China country code (0x26); its parser gates the
    // full signature itself, and T/UWA 005.2-1 defines the same byte layout
    // for the AV1 metadata OBU as for the HEVC SEI.
    if body[0] == 0x26 {
        if let Some(info) = sei::parse_hdr_vivid(body) {
            match &mut out.sei.hdr_vivid {
                Some(mine) => mine.absorb(&info),
                slot => *slot = Some(info),
            }
        }
        return;
    }
    if body[0] != 0xB5 {
        return;
    }
    let provider = u16::from_be_bytes([body[1], body[2]]);
    match provider {
        PROVIDER_DOLBY => {
            if let Some(rpu) = parse_av1_rpu(body) {
                out.rpus.push(rpu);
            }
        }
        PROVIDER_HDR10PLUS => {
            if let Some(info) = sei::parse_hdr10plus(body) {
                out.sei.hdr10plus.get_or_insert(info);
            }
        }
        _ => {}
    }
}

/// Read an unsigned LEB128, advancing `pos`. AV1 caps these at 8 bytes.
fn read_leb128(d: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for i in 0..8 {
        let b = *d.get(*pos)?;
        *pos += 1;
        value |= ((b & 0x7F) as u64) << (i * 7);
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_multibyte() {
        // 0x80 0x01 = 128.
        let mut pos = 0;
        assert_eq!(read_leb128(&[0x80, 0x01], &mut pos), Some(128));
        assert_eq!(pos, 2);
    }

    #[test]
    fn hdr_cll_metadata_obu() {
        // OBU header: type 5 (metadata), has_size_field. (5<<3)|0x02 = 0x2A.
        // payload: metadata_type=1 (CLL leb128) + MaxCLL 1000 + MaxFALL 400.
        let payload = [0x01u8, 0x03, 0xE8, 0x01, 0x90];
        let mut obu = vec![0x2A, payload.len() as u8];
        obu.extend_from_slice(&payload);
        let scan = scan_obus(&obu);
        let cl = scan.sei.content_light.expect("cll");
        assert_eq!(cl.max_cll, 1000);
        assert_eq!(cl.max_fall, 400);
    }

    #[test]
    fn hdr_mdcv_metadata_obu_real_bytes() {
        // Real HDR_MDCV OBU from dv10_av1: header 0x2A, size 0x1A(26), payload =
        // metadata_type 2 + 24-byte MDCV + trailing 0x80. AV1 fixed point:
        // luminance_max 0x00271000 / 256 = 10000 cd/m²; luminance_min 0x52 = 82
        // / 16384 ≈ 0.005. Primaries (R,G,B /65536): (0.708,0.292) (0.170,0.797)
        // (0.131,0.046), white (0.3127,0.329) → BT.2020 D65.
        let obu = [
            0x2A, 0x1A, 0x02, 0xb5, 0x3f, 0x4a, 0xc1, 0x2b, 0x85, 0xcc, 0x08, 0x21, 0x89, 0x0b,
            0xc7, 0x50, 0x0d, 0x54, 0x39, 0x00, 0x27, 0x10, 0x00, 0x00, 0x00, 0x00, 0x52, 0x80,
        ];
        let scan = scan_obus(&obu);
        let m = scan.sei.mastering.expect("mastering from MDCV");
        assert_eq!(m.max_luminance, 10000.0);
        assert_eq!(m.min_luminance, 0.005);
        assert_eq!(m.primaries.as_deref(), Some("BT.2020"));
    }

    #[test]
    fn non_dolby_t35_ignored() {
        // metadata_type 4 (T.35) with a non-Dolby, non-HDR10+ provider.
        let mut payload = vec![0x04u8, 0xB5, 0x00, 0x99];
        payload.extend_from_slice(&[0u8; 32]);
        let mut obu = vec![0x2A, payload.len() as u8];
        obu.extend_from_slice(&payload);
        let scan = scan_obus(&obu);
        assert!(scan.rpus.is_empty());
        assert!(scan.sei.hdr10plus.is_none());
    }
}
