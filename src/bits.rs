//! Minimal MSB-first bit reader for parsing codec headers (HEVC SPS, etc.).
//! Operates on an RBSP byte slice (emulation-prevention bytes already removed).

pub struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0..=7, number of bits already consumed in current byte
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, byte_pos: 0, bit_pos: 0 }
    }

    #[inline]
    pub fn read_bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.byte_pos)?;
        let bit = (byte >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Some(bit as u32)
    }

    /// Read `n` bits (n <= 32) as an unsigned big-endian value.
    pub fn read_bits(&mut self, n: u32) -> Option<u32> {
        let mut v: u32 = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()?;
        }
        Some(v)
    }

    /// Unsigned Exp-Golomb (ue(v)).
    pub fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        loop {
            let b = self.read_bit()?;
            if b == 1 {
                break;
            }
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        Some((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Signed Exp-Golomb (se(v)).
    pub fn read_se(&mut self) -> Option<i32> {
        let code = self.read_ue()?;
        let k = code.div_ceil(2) as i32;
        Some(if code % 2 == 1 { k } else { -k })
    }

    pub fn skip_bits(&mut self, n: u32) -> Option<()> {
        for _ in 0..n {
            self.read_bit()?;
        }
        Some(())
    }
}

/// Strip HEVC/AVC emulation-prevention bytes (00 00 03 -> 00 00) producing an RBSP.
pub fn ebsp_to_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0u32;
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if zeros >= 2 && b == 0x03 {
            // Drop the emulation-prevention byte; reset run.
            zeros = 0;
            i += 1;
            // The byte following 0x03 is emitted normally on next iterations.
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_unsigned() {
        // ue: '1'=0, '010'=1, '011'=2, '00100'=3.
        // Underscores mark the codeword boundaries, not nibbles.
        #[allow(clippy::unusual_byte_groupings)]
        let data = [0b1_010_011_0, 0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_ue(), Some(0));
        assert_eq!(r.read_ue(), Some(1));
        assert_eq!(r.read_ue(), Some(2));
        assert_eq!(r.read_ue(), Some(3));
    }

    #[test]
    fn ebsp_strips_emulation_prevention() {
        // 00 00 03 00 -> 00 00 00 ; trailing 03 kept when not preceded by 00 00.
        let input = [0x00, 0x00, 0x03, 0x00, 0x01, 0x03];
        assert_eq!(ebsp_to_rbsp(&input), vec![0x00, 0x00, 0x00, 0x01, 0x03]);
    }
}
