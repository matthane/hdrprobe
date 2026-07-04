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
    // The no-op tick body leaves `next_tick` dead, so the monomorphized copy
    // carries no gate at all — this stays the hot per-chunk path.
    split_annexb_impl(data, out, |_| {});
}

/// `split_annexb` with a byte-position callback every ~`TICK_BYTES`, for
/// progress on the `--full` whole-stream walk (a raw elementary stream can be
/// tens of GB, and this split is its only pass over the bytes).
pub fn split_annexb_ticked(data: &[u8], out: &mut Vec<NalRef>, tick: impl FnMut(usize)) {
    split_annexb_impl(data, out, tick);
}

/// Byte interval between `split_annexb_ticked` callbacks.
const TICK_BYTES: usize = 2 << 20;

#[inline]
fn split_annexb_impl(data: &[u8], out: &mut Vec<NalRef>, mut tick: impl FnMut(usize)) {
    let n = data.len();
    let mut i = 0usize;
    let mut next_tick = TICK_BYTES;
    // If the buffer doesn't begin with a start code, treat offset 0 as an
    // implicit NAL boundary. (Our access-unit chunks start at a NAL header,
    // not a start code, so the first NAL would otherwise be dropped.)
    let starts_with_sc = n >= 3 && data[0] == 0 && data[1] == 0 && (data[2] == 1 || (n >= 4 && data[2] == 0 && data[3] == 1));
    let mut nal_start: Option<usize> = if starts_with_sc { None } else { Some(0) };
    while i + 3 <= n {
        if i >= next_tick {
            tick(i);
            next_tick = i + TICK_BYTES;
        }
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
    fn ticked_split_matches_plain_and_fires() {
        // A buffer past the tick interval: identical NAL list either way, and
        // the callback reports monotonically increasing positions.
        let mut data = vec![0u8; TICK_BYTES + 4096];
        for (i, sc) in [0usize, 1000, TICK_BYTES - 100, TICK_BYTES + 500].iter().enumerate() {
            data[*sc..*sc + 4].copy_from_slice(&[0, 0, 1, (33 << 1) ^ (i as u8 & 1)]);
        }
        let (mut plain, mut ticked) = (Vec::new(), Vec::new());
        split_annexb(&data, &mut plain);
        let mut ticks: Vec<usize> = Vec::new();
        split_annexb_ticked(&data, &mut ticked, |pos| ticks.push(pos));
        assert_eq!(plain.len(), ticked.len());
        assert!(plain
            .iter()
            .zip(&ticked)
            .all(|(a, b)| (a.start, a.end, a.nal_type) == (b.start, b.end, b.nal_type)));
        assert!(!ticks.is_empty(), "a walk past TICK_BYTES must tick");
        assert!(ticks.windows(2).all(|w| w[0] < w[1]));
        assert!(ticks.iter().all(|&p| p <= data.len()));
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
