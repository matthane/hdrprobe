//! AVC (H.264) NAL handling: Annex-B start-code splitting, length-prefixed
//! (avcC) splitting, and NAL-type classification.
//!
//! Structurally parallel to [`crate::hevc::nal`] — the start-code scanning is
//! identical, but the AVC NAL header is **one byte** (`forbidden_zero_bit(1) +
//! nal_ref_idc(2) + nal_unit_type(5)`) rather than HEVC's two, so `nal_type`
//! reads the low 5 bits of a single byte.

/// A NAL unit, referenced by byte range into the underlying buffer. The range
/// includes the 1-byte NAL header but excludes any start code / length prefix.
#[derive(Debug, Clone, Copy)]
pub struct NalRef {
    pub nal_type: u8,
    pub start: usize,
    pub end: usize,
}

// AVC NAL unit types of interest.
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;

/// Whether `nal_type` is in the H.264 *unspecified* range (24..=31), where Dolby
/// Vision carries its RPU (Dolby uses type 28). We deliberately don't hard-code
/// 28: a NAL in this range is only treated as an RPU once its payload is
/// content-verified against the `0x19` `rpu_nal_prefix` and libdovi's CRC (see
/// the sampler), so an atypical mux using another unspecified type still parses
/// and, conversely, a non-DV unspecified NAL is never misread as an RPU.
#[inline]
pub fn is_unspecified(nal_type: u8) -> bool {
    (24..=31).contains(&nal_type)
}

#[inline]
fn nal_type(header_byte: u8) -> u8 {
    header_byte & 0x1F
}

/// Split an Annex-B byte stream into NAL units (3- or 4-byte start codes).
/// Mirrors [`crate::hevc::nal::split_annexb`]; only the header decode differs.
pub fn split_annexb(data: &[u8], out: &mut Vec<NalRef>) {
    let n = data.len();
    let mut i = 0usize;
    // If the buffer doesn't begin with a start code, treat offset 0 as an
    // implicit NAL boundary (access-unit chunks start at a NAL header).
    let starts_with_sc = n >= 3
        && data[0] == 0
        && data[1] == 0
        && (data[2] == 1 || (n >= 4 && data[2] == 0 && data[3] == 1));
    let mut nal_start: Option<usize> = if starts_with_sc { None } else { Some(0) };
    while i + 3 <= n {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let payload = i + 3;
            if let Some(prev) = nal_start.take() {
                push_nal(data, prev, i, out);
            }
            nal_start = Some(payload);
            i = payload;
        } else {
            i += 1;
        }
    }
    if let Some(prev) = nal_start.take() {
        push_nal(data, prev, n, out);
    }
}

fn push_nal(data: &[u8], start: usize, mut end: usize, out: &mut Vec<NalRef>) {
    // Trim a trailing zero byte that belongs to the next 4-byte start code.
    while end > start && data[end - 1] == 0 {
        end -= 1;
    }
    if end <= start {
        return;
    }
    out.push(NalRef { nal_type: nal_type(data[start]), start, end });
}

/// Split a length-prefixed (avcC / ISOBMFF) sample into NAL units. `nlen` is the
/// NAL length field size in bytes (1..=4, from avcC `lengthSizeMinusOne + 1`).
pub fn split_length_prefixed(data: &[u8], nlen: u8, out: &mut Vec<NalRef>) {
    let nlen = nlen as usize;
    let mut i = 0usize;
    while i + nlen <= data.len() {
        let mut len = 0usize;
        for k in 0..nlen {
            len = (len << 8) | data[i + k] as usize;
        }
        let start = i + nlen;
        let end = start + len;
        if len == 0 || end > data.len() {
            break;
        }
        out.push(NalRef { nal_type: nal_type(data[start]), start, end });
        i = end;
    }
}

/// Locate the first SPS NAL (type 7) inside an `avcC`
/// (AVCDecoderConfigurationRecord) payload / MKV CodecPrivate. Returns the NAL
/// bytes including the 1-byte NAL header.
pub fn find_sps_in_avcc(avcc: &[u8]) -> Option<&[u8]> {
    // Fixed part: configurationVersion(8), AVCProfileIndication(8),
    // profile_compatibility(8), AVCLevelIndication(8), reserved(6)+
    // lengthSizeMinusOne(2), reserved(3)+numOfSequenceParameterSets(5).
    if avcc.len() < 6 {
        return None;
    }
    let num_sps = avcc[5] & 0x1F;
    let mut p = 6usize;
    for _ in 0..num_sps {
        if p + 2 > avcc.len() {
            return None;
        }
        let len = u16::from_be_bytes([avcc[p], avcc[p + 1]]) as usize;
        p += 2;
        if p + len > avcc.len() {
            return None;
        }
        if len > 0 && nal_type(avcc[p]) == NAL_SPS {
            return Some(&avcc[p..p + len]);
        }
        p += len;
    }
    None
}

/// The `lengthSizeMinusOne + 1` NAL length field size from an `avcC` record.
pub fn avcc_nal_len(avcc: &[u8]) -> Option<u8> {
    avcc.get(4).map(|b| (b & 0x03) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_splits_and_types_avc() {
        // [SC4][SPS type7 + 2][SC3][DV RPU type28 + prefix 0x19]
        // SPS header 0|11|00111 = 0x67; RPU (type 28) 0|00|11100 = 0x1C.
        let data = [0, 0, 0, 1, 0x67, 0xAA, 0xBB, 0, 0, 1, 0x1C, 0x19, 0x08];
        let mut out = Vec::new();
        split_annexb(&data, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].nal_type, NAL_SPS);
        assert_eq!(out[1].nal_type, 28);
        assert!(is_unspecified(out[1].nal_type));
        // RPU payload begins with the 0x19 rpu_nal_prefix right after the header.
        assert_eq!(data[out[1].start], 0x1C);
        assert_eq!(data[out[1].start + 1], 0x19);
    }

    #[test]
    fn length_prefixed_avc() {
        // [len=3][SPS 0x67 + 2] [len=2][RPU 0x1C + 1]
        let data = [0, 0, 0, 3, 0x67, 0xAA, 0xBB, 0, 0, 0, 2, 0x1C, 0x19];
        let mut out = Vec::new();
        split_length_prefixed(&data, 4, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].nal_type, NAL_SPS);
        assert_eq!(out[1].nal_type, 28);
        assert!(is_unspecified(out[1].nal_type));
    }

    #[test]
    fn find_sps_in_avcc_record() {
        // avcC: ver=1, prof=100, compat=0, level=42, ff|len(=4-1=3)=0xFF,
        // e0|numSPS(=1)=0xE1, [len=3][0x67 SPS + 2 bytes]
        let avcc = [1, 100, 0, 42, 0xFF, 0xE1, 0, 3, 0x67, 0x42, 0x80];
        let sps = find_sps_in_avcc(&avcc).expect("sps present");
        assert_eq!(sps[0] & 0x1F, NAL_SPS);
        assert_eq!(avcc_nal_len(&avcc), Some(4));
    }
}
