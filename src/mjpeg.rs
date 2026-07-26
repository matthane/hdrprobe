//! Motion JPEG, read from the frame's own JPEG (ITU-T T.81) markers.
//!
//! MJPEG is a sequence of ordinary JPEG images, so the two facts the report
//! can state — bit depth and chroma sampling — sit in the frame header
//! (`SOF`, T.81 §B.2.2) a few marker segments into any chunk, and every frame
//! carries one. The walk is bounded the way every marker-structured parse here
//! is: each non-standalone marker segment declares its own 16-bit length
//! (T.81 §B.1.1.4), the standalone markers (`SOI`, `TEM`, `RST0`–`RST7`) have
//! none, and the walk stops at `SOS` — past which entropy-coded data begins
//! and a naive scan would read compressed bytes as markers — or after
//! [`MAX_MARKERS`] segments.
//!
//! Two facts shape what is and is not reported.
//!
//! **Bit depth and chroma are signalled, not family constants.** The `SOF`
//! precision byte is 8 for baseline (`SOF0`, T.81 §B.2.2 fixes it) but 8 or 12
//! for the extended and progressive frames and 2..16 for lossless, and the
//! per-component sampling factors distinguish 4:2:0 from the 4:2:2 that real
//! capture hardware routinely writes — so unlike the WMV/MS-MPEG-4 constants
//! beside this module, these values are read, never assumed. Sampling is
//! reported only for the layouts whose name is settled (luma factors over
//! 1×1 chroma, or a single-component image as `monochrome`); an exotic
//! combination fills nothing rather than guessing.
//!
//! **MJPEG records no colour anywhere hdrprobe reads.** The JFIF `APP0`
//! segment carries aspect and thumbnail data only, and while JFIF (ITU-T
//! T.871) *defines* its YCbCr as BT.601 full-range, that definition is about
//! the JFIF interchange format rather than a signal in the stream; ffprobe's
//! `yuvj420p` reflects the same convention. The Color line stays empty like
//! every other format that records none — the settled rule from the SDR
//! coverage plan.

/// What a `SOF` frame header states: the sample precision and, where the
/// component layout names one, the chroma sampling.
pub struct SofInfo {
    /// `P`, the sample precision in bits (T.81 §B.2.2).
    pub precision: u8,
    /// Chroma sampling derived from the per-component sampling factors, or
    /// `None` for a layout with no settled name.
    pub chroma: Option<&'static str>,
}

/// Marker segments walked before giving up. A real frame reaches its `SOF`
/// within a handful (the corpus file: APP0, COM, DQT, DHT, then SOF0); the
/// bound exists so a malformed length chain cannot walk far.
const MAX_MARKERS: usize = 64;

/// Parse the JPEG frame header out of one MJPEG chunk (a whole JPEG image).
///
/// `None` when the chunk does not open with `SOI`, when the marker chain is
/// malformed, or when `SOS`/`EOI` arrives before any `SOF` — each meaning the
/// bytes cannot be read as the structure above, so nothing is guessed.
pub fn parse_frame_header(data: &[u8]) -> Option<SofInfo> {
    if data.get(..2)? != [0xFF, 0xD8] {
        return None; // not SOI: this chunk is not a JPEG image
    }
    let mut p = 2usize;
    for _ in 0..MAX_MARKERS {
        // T.81 §B.1.1.2: a marker may be preceded by any number of 0xFF fill
        // bytes.
        while data.get(p) == Some(&0xFF) && data.get(p + 1) == Some(&0xFF) {
            p += 1;
        }
        if *data.get(p)? != 0xFF {
            return None; // lost the marker chain
        }
        match *data.get(p + 1)? {
            // Standalone markers: SOI (again), TEM, RST0..RST7.
            0xD8 | 0x01 | 0xD0..=0xD7 => p += 2,
            // SOS or EOI before any SOF: entropy-coded data (or the image's
            // end) begins and no frame header was seen.
            0xDA | 0xD9 => return None,
            // SOF0..SOF3: baseline, extended sequential, progressive,
            // lossless. The arithmetic-coded and hierarchical SOF variants
            // (C5..C7, C9..CB, CD..CF) share the layout but never appear in
            // MJPEG; they fall to the skip arm below and the walk ends at SOS
            // with nothing filled, which is the honest outcome for a frame
            // this module has no witness for.
            0xC0..=0xC3 => {
                let len = be16(data, p + 2)? as usize;
                // §B.2.2: Lf is 8 + 3×Nf.
                let seg = data.get(p + 4..(p + 2).checked_add(len)?)?;
                let precision = *seg.first()?;
                let n = *seg.get(5)? as usize;
                let comps = seg.get(6..6 + 3 * n)?;
                return Some(SofInfo { precision, chroma: chroma_from_components(comps, n) });
            }
            _ => {
                let len = be16(data, p + 2)? as usize;
                if len < 2 {
                    return None; // a segment length cannot undercut itself
                }
                p = p.checked_add(2 + len)?;
            }
        }
    }
    None
}

/// Name the chroma sampling from the `SOF` component list — `(C, H|V, Tq)`
/// triplets per §B.2.2.
///
/// Subsampling is the *ratio* of the luma factors to the chroma factors, not
/// the factors themselves: ffmpeg's 4:4:4 encodes write a uniform `0x12` on
/// all three components (measured on `testfiles/sdr/mjpeg.mov`), which is
/// unsubsampled chroma exactly as `0x11` everywhere would be. So the two
/// chroma components must match each other, each luma factor must divide the
/// chroma's, and the quotient pair names the layout. One component is
/// `monochrome`; anything else — mismatched chroma, a non-integer ratio, a
/// quotient with no settled name — fills nothing.
fn chroma_from_components(c: &[u8], n: usize) -> Option<&'static str> {
    match n {
        1 => Some("monochrome"),
        3 => {
            let (y, cb, cr) = (c[1], c[4], c[7]);
            if cb != cr {
                return None;
            }
            let (yh, yv) = (u32::from(y >> 4), u32::from(y & 0xF));
            let (ch, cv) = (u32::from(cb >> 4), u32::from(cb & 0xF));
            if ch == 0 || cv == 0 || yh % ch != 0 || yv % cv != 0 {
                return None;
            }
            match (yh / ch, yv / cv) {
                (1, 1) => Some("4:4:4"),
                (2, 2) => Some("4:2:0"),
                (2, 1) => Some("4:2:2"),
                (4, 1) => Some("4:1:1"),
                (1, 2) => Some("4:4:0"),
                _ => None,
            }
        }
        _ => None,
    }
}

fn be16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus file's own preamble (`testfiles/sdr/mjpeg.avi`, first chunk):
    /// SOI, APP0 (JFIF), COM, then a minimal DQT stand-in and the real SOF0 —
    /// segment lengths and the SOF payload byte-for-byte from the file.
    fn frame(sof_body: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8]; // SOI
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]); // APP0, len 16
        v.extend_from_slice(b"JFIF\0");
        v.extend_from_slice(&[0; 9]);
        v.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x04, 0x41, 0x42]); // COM
        v.extend_from_slice(&[0xFF, 0xC0]); // SOF0
        v.extend_from_slice(&((sof_body.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(sof_body);
        v
    }

    /// `testfiles/sdr/mjpeg.avi`'s SOF0 payload: precision 8, 320×240, Y 2×2
    /// over 1×1 chroma — 4:2:0.
    const SOF_420: [u8; 15] = [
        8, 0x00, 0xF0, 0x01, 0x40, 3, 1, 0x22, 0, 2, 0x11, 0, 3, 0x11, 0,
    ];

    #[test]
    fn reads_the_corpus_frames_precision_and_sampling() {
        let s = parse_frame_header(&frame(&SOF_420)).expect("parses");
        assert_eq!(s.precision, 8);
        assert_eq!(s.chroma, Some("4:2:0"));
    }

    #[test]
    fn names_the_settled_layouts_and_declines_the_rest() {
        for (y, want) in [
            (0x21, Some("4:2:2")),
            (0x11, Some("4:4:4")),
            (0x41, Some("4:1:1")),
            (0x12, Some("4:4:0")),
            // A ratio with no settled name fills nothing.
            (0x33, None),
        ] {
            let mut body = SOF_420;
            body[7] = y;
            let s = parse_frame_header(&frame(&body)).expect("parses");
            assert_eq!(s.chroma, want, "luma factors {y:#04x}");
        }
        // The naming is a ratio, not the raw factors: ffmpeg's 4:4:4 writes a
        // uniform 0x12 on all three components (`testfiles/sdr/mjpeg.mov`).
        let mut body = SOF_420;
        (body[7], body[10], body[13]) = (0x12, 0x12, 0x12);
        assert_eq!(parse_frame_header(&frame(&body)).unwrap().chroma, Some("4:4:4"));
        // Mismatched chroma components have no settled name.
        let mut body = SOF_420;
        body[10] = 0x21;
        assert_eq!(parse_frame_header(&frame(&body)).unwrap().chroma, None);
        // A luma factor the chroma's does not divide has none either.
        let mut body = SOF_420;
        (body[7], body[10], body[13]) = (0x32, 0x22, 0x22);
        assert_eq!(parse_frame_header(&frame(&body)).unwrap().chroma, None);
        // One component is monochrome; its 12-bit form keeps its precision.
        let mono = [12, 0x00, 0xF0, 0x01, 0x40, 1, 1, 0x11, 0];
        let s = parse_frame_header(&frame(&mono)).expect("parses");
        assert_eq!((s.precision, s.chroma), (12, Some("monochrome")));
    }

    #[test]
    fn refuses_what_is_not_a_readable_frame() {
        // Not a JPEG at all.
        assert!(parse_frame_header(&[0x00, 0x00, 0x01, 0xB3]).is_none());
        // SOS before any SOF: entropy data would follow, stop clean.
        assert!(parse_frame_header(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]).is_none());
        // A segment length that undercuts itself cannot advance.
        assert!(parse_frame_header(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x01]).is_none());
        // A declared length past the buffer reads nothing.
        assert!(parse_frame_header(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF, 0x00]).is_none());
        // A truncated SOF yields nothing rather than a partial answer.
        let mut f = frame(&SOF_420);
        f.truncate(f.len() - 4);
        assert!(parse_frame_header(&f).is_none());
        // The marker-count bound ends a fill-byte flood.
        let mut flood = vec![0xFF, 0xD8];
        flood.extend(std::iter::repeat_n([0xFF, 0x01], 100).flatten());
        assert!(parse_frame_header(&flood).is_none());
    }
}
