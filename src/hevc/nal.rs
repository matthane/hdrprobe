//! HEVC NAL handling: Annex-B start-code splitting, length-prefixed (HVCC)
//! splitting, and NAL-type classification.

/// A NAL unit, referenced by byte range into the underlying buffer. The range
/// includes the 2-byte NAL header but excludes any start code / length prefix.
#[derive(Debug, Clone, Copy)]
pub struct NalRef {
    pub nal_type: u8,
    pub start: usize,
    pub end: usize,
}

// HEVC NAL unit types of interest.
pub const NAL_SPS: u8 = 33;
pub const NAL_PREFIX_SEI: u8 = 39;
pub const NAL_SUFFIX_SEI: u8 = 40;
pub const NAL_UNSPEC62_RPU: u8 = 62;

#[inline]
fn nal_type(header_byte: u8) -> u8 {
    (header_byte >> 1) & 0x3F
}

/// Split an Annex-B byte stream into NAL units (3- or 4-byte start codes).
/// `base` is the offset of `data` within the file, recorded into the ranges.
pub fn split_annexb(data: &[u8], out: &mut Vec<NalRef>) {
    let n = data.len();
    let mut i = 0usize;
    // If the buffer doesn't begin with a start code, treat offset 0 as an
    // implicit NAL boundary. (Our access-unit chunks start at a NAL header,
    // not a start code, so the first NAL would otherwise be dropped.)
    let starts_with_sc = n >= 3 && data[0] == 0 && data[1] == 0 && (data[2] == 1 || (n >= 4 && data[2] == 0 && data[3] == 1));
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
    let t = nal_type(data[start]);
    out.push(NalRef { nal_type: t, start, end });
}

#[cfg(test)]
mod tests {
    use super::*;

    // NAL header for type 33 (SPS): (33<<1)=0x42. Type 62 (RPU): 0x7C.
    #[test]
    fn annexb_4byte_and_3byte_start_codes() {
        // [SC4][type33 + 2 payload][SC3][type62 + 1 payload]
        let data = [
            0, 0, 0, 1, 0x42, 0x01, 0xAA, 0xBB, 0, 0, 1, 0x7C, 0x01, 0x19,
        ];
        let mut out = Vec::new();
        split_annexb(&data, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].nal_type, 33);
        assert_eq!(out[1].nal_type, 62);
        // Second NAL must include its 2-byte header starting at 0x7C.
        assert_eq!(data[out[1].start], 0x7C);
    }

    #[test]
    fn annexb_implicit_first_nal_without_start_code() {
        // Chunk begins at a NAL header (no leading start code), then a 3-byte SC.
        let data = [0x7C, 0x01, 0x19, 0xDE, 0, 0, 1, 0x42, 0x01];
        let mut out = Vec::new();
        split_annexb(&data, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].nal_type, 62);
        assert_eq!(out[1].nal_type, 33);
    }

    #[test]
    fn length_prefixed_four_byte() {
        // [len=3][type62 hdr + 1] [len=2][type33 hdr]
        let data = [
            0, 0, 0, 3, 0x7C, 0x01, 0x19, 0, 0, 0, 2, 0x42, 0x01,
        ];
        let mut out = Vec::new();
        split_length_prefixed(&data, 4, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].nal_type, 62);
        assert_eq!(out[1].nal_type, 33);
    }
}

/// Split a length-prefixed (HVCC / ISOBMFF) sample into NAL units.
/// `nlen` is the NAL length field size in bytes (1..=4, from hvcC).
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
        let t = nal_type(data[start]);
        out.push(NalRef { nal_type: t, start, end });
        i = end;
    }
}
