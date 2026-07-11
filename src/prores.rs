//! ProRes frame-header + profile parsing. ProRes has no SEI/OBU/RPU side
//! channel at all — static HDR and colour authority ride the container (MKV
//! `Colour`, MOV/MP4 `colr`/`mdcv`/`clli`), and dynamic HDR does not exist for
//! ProRes carriage (DV masters pair with CM XML sidecars) — so this module only
//! ever supplements container signalling, never overrides it. The frame header
//! does carry its own CICP colour bytes, but real encodes routinely leave them
//! unspecified (the corpus MKV writes 2/2/6 under real BT.2020/PQ container
//! signalling), which is exactly why the container keeps authority.
//!
//! The profile (422 Proxy/LT/422/HQ, 4444, 4444 XQ) is signalled only by the
//! MOV/MP4 sample-entry FourCC; the frame header names the encoder, not the
//! profile, and Matroska's `V_PRORES` carries no FourCC anywhere — so an MKV
//! mux gets no profile label (MediaInfo/ffprobe agree), never a guess.

use crate::model::ColorInfo;

/// What a ProRes frame header declares. Chroma format is the frame's own word;
/// bit depth is the profile family's defined depth (the header has no depth
/// field): every 4:2:2 profile codes 10-bit, the 4444 family codes 12-bit.
/// Dimensions are parsed only as a plausibility gate — the container states
/// them authoritatively, so they aren't returned.
pub struct ProresFrameInfo {
    pub chroma: &'static str,
    pub bit_depth: u8,
    /// The header's own CICP primaries/transfer/matrix, mapped through the
    /// shared label tables — `None` per field for unspecified/unknown codes.
    /// The header has no range field, so `range` is always `None`.
    pub color: ColorInfo,
}

/// Parse the ProRes frame header at byte 0. Accepts both carriage forms: a
/// MOV/MP4 sample (the full `frame_size + 'icpf'` atom) and a Matroska
/// `V_PRORES` block (the same frame with that 8-byte atom header stripped, per
/// the Matroska codec spec) — detected by the `icpf` tag, never by guessing.
/// Returns `None` on anything implausible; every frame is intra-coded and
/// carries the header, so callers only ever need the first parseable chunk.
pub fn parse_frame_header(data: &[u8]) -> Option<ProresFrameInfo> {
    let body = if data.get(4..8) == Some(b"icpf") { &data[8..] } else { data };
    if body.len() < 20 {
        return None;
    }
    let hdr_size = u16::from_be_bytes([body[0], body[1]]);
    // 20 fixed bytes precede the quant matrices the size also spans.
    if (hdr_size as usize) < 20 {
        return None;
    }
    // body[2..4] version, body[4..8] encoder id (e.g. ffmpeg's 'fmpg') — the
    // creator, not the profile; nothing in the report reads either.
    // Dimensions gate plausibility only; the container's word is authoritative.
    if body[8..10] == [0, 0] || body[10..12] == [0, 0] {
        return None;
    }
    let (chroma_fmt, bit_depth) = match body[12] >> 6 {
        2 => ("4:2:2", 10),
        3 => ("4:4:4", 12),
        _ => return None, // reserved chroma codes: not a ProRes frame header
    };
    // Low nibble of the src-pix-fmt byte: alpha channel presence (1 = 8-bit,
    // 2 = 16-bit). Only the 4444 family carries one.
    let chroma = if chroma_fmt == "4:4:4" && matches!(body[17] & 0x0F, 1 | 2) {
        "4:4:4:4"
    } else {
        chroma_fmt
    };
    let color = ColorInfo {
        primaries: crate::container::cicp_primaries(body[14] as u16).map(str::to_string),
        transfer: crate::container::cicp_transfer(body[15] as u16).map(str::to_string),
        matrix: crate::container::cicp_matrix(body[16] as u16).map(str::to_string),
        range: None,
    };
    Some(ProresFrameInfo { chroma, bit_depth, color })
}

/// The report's profile label plus the family's defined chroma/bit depth, from
/// the MOV/MP4 sample-entry FourCC. `None` for FourCCs outside the six ProRes
/// video profiles — ProRes RAW (`aprn`/`aprh`) is a different codec family and
/// stays on the generic fallback.
pub fn profile_from_fourcc(fourcc: &[u8; 4]) -> Option<(&'static str, &'static str, u8)> {
    Some(match fourcc {
        b"apco" => ("422 Proxy", "4:2:2", 10),
        b"apcs" => ("422 LT", "4:2:2", 10),
        b"apcn" => ("422", "4:2:2", 10),
        b"apch" => ("422 HQ", "4:2:2", 10),
        b"ap4h" => ("4444", "4:4:4", 12),
        b"ap4x" => ("4444 XQ", "4:4:4", 12),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The corpus MKV's first block head (V_PRORES: the 8-byte size+'icpf' atom
    // is stripped): hdr_size 148, version 0, 'fmpg', 3840×2160, frame_flags
    // 0x83 (4:2:2 progressive), CICP 2/2/6 — colour left unspecified by the
    // encoder while the real signalling rides the MKV Colour element.
    const CORPUS_HEADER: [u8; 20] = [
        0x00, 0x94, 0x00, 0x00, 0x66, 0x6d, 0x70, 0x67, 0x0f, 0x00, 0x08, 0x70, 0x83, 0x00,
        0x02, 0x02, 0x06, 0x20, 0x00, 0x03,
    ];

    #[test]
    fn corpus_mkv_block_header_parses() {
        let f = parse_frame_header(&CORPUS_HEADER).expect("corpus header");
        assert_eq!(f.chroma, "4:2:2");
        assert_eq!(f.bit_depth, 10);
        // CICP 2/2 are unspecified and matrix 6 has no shared-table label, so
        // all three stay None: the container must classify this file, the
        // header can't.
        assert!(f.color.primaries.is_none());
        assert!(f.color.transfer.is_none());
        assert!(f.color.matrix.is_none());
        assert!(f.color.range.is_none(), "the header has no range field");
    }

    #[test]
    fn mov_sample_form_carries_the_icpf_atom() {
        // A MOV/MP4 sample keeps the 8-byte frame atom header in front.
        let mut sample = Vec::from(&[0x00, 0x28, 0xE9, 0x50][..]); // frame_size
        sample.extend_from_slice(b"icpf");
        sample.extend_from_slice(&CORPUS_HEADER);
        let f = parse_frame_header(&sample).expect("atom-prefixed form");
        assert_eq!((f.chroma, f.bit_depth), ("4:2:2", 10));
    }

    #[test]
    fn header_colour_maps_through_the_cicp_tables() {
        // A header that does state its colour: BT.2020 / PQ / BT.2020 NCL.
        let mut h = CORPUS_HEADER;
        h[14] = 9;
        h[15] = 16;
        h[16] = 9;
        let f = parse_frame_header(&h).expect("cicp header");
        assert_eq!(f.color.primaries.as_deref(), Some("BT.2020"));
        assert_eq!(f.color.transfer.as_deref(), Some("PQ (SMPTE ST 2084)"));
        assert_eq!(f.color.matrix.as_deref(), Some("BT.2020 NCL"));
    }

    #[test]
    fn quad_chroma_and_alpha() {
        // 4444 family: frame_flags 0xC0 (4:4:4), 12-bit; alpha_info marks the
        // fourth channel.
        let mut h = CORPUS_HEADER;
        h[12] = 0xC0;
        let f = parse_frame_header(&h).expect("4:4:4 header");
        assert_eq!((f.chroma, f.bit_depth), ("4:4:4", 12));
        h[17] = 0x21; // 8-bit alpha
        let f = parse_frame_header(&h).expect("alpha header");
        assert_eq!(f.chroma, "4:4:4:4");
        // An alpha nibble on a 4:2:2 frame is not a thing — ignored.
        h[12] = 0x83;
        let f = parse_frame_header(&h).expect("4:2:2 header");
        assert_eq!(f.chroma, "4:2:2");
    }

    #[test]
    fn garbage_and_truncation_are_rejected() {
        assert!(parse_frame_header(&[]).is_none());
        assert!(parse_frame_header(&CORPUS_HEADER[..19]).is_none());
        let mut h = CORPUS_HEADER;
        h[0] = 0;
        h[1] = 8; // hdr_size below the fixed 20 bytes
        assert!(parse_frame_header(&h).is_none());
        let mut h = CORPUS_HEADER;
        h[12] = 0x03; // reserved chroma code 0
        assert!(parse_frame_header(&h).is_none());
        let mut h = CORPUS_HEADER;
        h[8] = 0;
        h[9] = 0; // zero width
        assert!(parse_frame_header(&h).is_none());
    }

    #[test]
    fn profile_table_covers_the_six_video_fourccs() {
        assert_eq!(profile_from_fourcc(b"apch"), Some(("422 HQ", "4:2:2", 10)));
        assert_eq!(profile_from_fourcc(b"apcn"), Some(("422", "4:2:2", 10)));
        assert_eq!(profile_from_fourcc(b"apcs"), Some(("422 LT", "4:2:2", 10)));
        assert_eq!(profile_from_fourcc(b"apco"), Some(("422 Proxy", "4:2:2", 10)));
        assert_eq!(profile_from_fourcc(b"ap4h"), Some(("4444", "4:4:4", 12)));
        assert_eq!(profile_from_fourcc(b"ap4x"), Some(("4444 XQ", "4:4:4", 12)));
        // ProRes RAW is a different codec family: generic fallback.
        assert_eq!(profile_from_fourcc(b"aprh"), None);
    }
}
