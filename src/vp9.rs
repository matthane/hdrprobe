//! VP9 bitstream + codec-config parsing: the uncompressed frame header (the
//! only in-band metadata VP9 has), the WebM CodecPrivate feature list, and the
//! shared profile label. VP9 carries no SEI/OBU side channel — colour beyond
//! the frame header's `color_space`/`color_range`, static HDR, and HDR10+ all
//! ride the container (MKV `Colour`, MP4 `colr`/`vpcC`, MKV BlockAdditions) —
//! so this module only ever supplements container signalling, never overrides
//! it.

use crate::bits::BitReader;
use crate::model::ColorInfo;

/// What a VP9 keyframe's uncompressed header declares. Every field is the
/// stream's own word; profiles 0/2 imply 4:2:0 (no subsampling bits) and
/// profiles 1/3 signal it explicitly.
pub struct Vp9FrameInfo {
    pub profile: u8,
    pub bit_depth: u8,
    pub chroma: &'static str,
    /// Matrix/range only (plus RGB's implied 4:4:4 full-range): the header's
    /// 3-bit `color_space` names a matrix family, never a transfer or
    /// primaries, so an HDR VP9 stream is classifiable only via its container.
    pub color: ColorInfo,
    pub width: u32,
    pub height: u32,
}

/// Parse the uncompressed header of the VP9 frame at byte 0. A superframe's
/// index lives at the *end* of the buffer, so byte 0 is always the first
/// frame's header and no superframe handling is needed. Returns `None` for
/// inter frames (their header carries no colour config) and anything that
/// fails the marker/sync gates — callers try successive access units until a
/// keyframe parses.
pub fn parse_frame_header(data: &[u8]) -> Option<Vp9FrameInfo> {
    let mut r = BitReader::new(data);
    if r.read_bits(2)? != 0b10 {
        return None; // frame_marker
    }
    let profile = (r.read_bit()? | (r.read_bit()? << 1)) as u8;
    if profile == 3 && r.read_bit()? != 0 {
        return None; // reserved_zero
    }
    if r.read_bit()? == 1 {
        return None; // show_existing_frame: no header follows
    }
    let frame_type = r.read_bit()?;
    let _show_frame = r.read_bit()?;
    let _error_resilient = r.read_bit()?;
    if frame_type != 0 {
        return None; // inter frame: no colour config
    }
    if r.read_bits(24)? != 0x49_8342 {
        return None; // frame_sync_code
    }

    // color_config
    let bit_depth = if profile >= 2 {
        if r.read_bit()? == 1 {
            12
        } else {
            10
        }
    } else {
        8
    };
    let color_space = r.read_bits(3)?;
    let (chroma, color) = if color_space != CS_RGB {
        let full_range = r.read_bit()? == 1;
        let chroma = if profile == 1 || profile == 3 {
            let ss_x = r.read_bit()?;
            let ss_y = r.read_bit()?;
            if r.read_bit()? != 0 {
                return None; // reserved_zero
            }
            chroma_str(ss_x, ss_y)
        } else {
            "4:2:0"
        };
        (chroma, color_info(color_space, full_range))
    } else {
        // CS_RGB (sRGB): only profiles 1/3 admit it — 4:4:4, full range.
        if profile == 1 || profile == 3 {
            if r.read_bit()? != 0 {
                return None; // reserved_zero
            }
        } else {
            return None;
        }
        ("4:4:4", color_info(CS_RGB, true))
    };

    // frame_size
    let width = r.read_bits(16)? + 1;
    let height = r.read_bits(16)? + 1;

    Some(Vp9FrameInfo { profile, bit_depth, chroma, color, width, height })
}

// The header's 3-bit color_space values (VP9 spec §7.2.2).
const CS_BT_601: u32 = 1;
const CS_BT_709: u32 = 2;
const CS_SMPTE_170: u32 = 3;
const CS_SMPTE_240: u32 = 4;
const CS_BT_2020: u32 = 5;
const CS_RGB: u32 = 7;

fn chroma_str(ss_x: u32, ss_y: u32) -> &'static str {
    match (ss_x, ss_y) {
        (1, 1) => "4:2:0",
        (1, 0) => "4:2:2",
        (0, 0) => "4:4:4",
        // 4:4:0 — legal in the header but unused by real encoders; the schema's
        // reserved-signalling rendering.
        _ => "?",
    }
}

/// Matrix + range from the header's color_space, mapped through the shared
/// CICP label tables (ffmpeg's mapping: BT601→5, BT709→1, SMPTE170→6,
/// SMPTE240→7, BT2020→9, RGB→0) so the label value space stays the one the
/// containers use. Codes without a table label (the BT.601 family) yield a
/// range-only `ColorInfo` — never a made-up name.
fn color_info(color_space: u32, full_range: bool) -> ColorInfo {
    let cicp = match color_space {
        CS_BT_601 => Some(5u16),
        CS_BT_709 => Some(1),
        CS_SMPTE_170 => Some(6),
        CS_SMPTE_240 => Some(7),
        CS_BT_2020 => Some(9),
        CS_RGB => Some(0),
        _ => None, // CS_UNKNOWN / reserved: nothing signalled
    };
    ColorInfo {
        primaries: None,
        transfer: None,
        matrix: cicp.and_then(crate::container::cicp_matrix).map(str::to_string),
        range: Some(if full_range { "full" } else { "limited" }.to_string()),
    }
}

pub struct Vp9CodecPrivate {
    pub profile: Option<u8>,
    pub level: Option<u8>,
    pub bit_depth: Option<u8>,
    pub chroma: Option<&'static str>,
}

/// Parse the WebM VP9 CodecPrivate: a feature list of `(id u8, len u8, value)`
/// triplets — 1 = profile, 2 = level, 3 = bit depth, 4 = chroma subsampling
/// (0/1 = 4:2:0 vertical/colocated, 2 = 4:2:2, 3 = 4:4:4). Many muxes omit it
/// entirely (mkvmerge wrote none before ~v30), so every field is optional.
pub fn parse_webm_codec_private(data: &[u8]) -> Vp9CodecPrivate {
    let mut out =
        Vp9CodecPrivate { profile: None, level: None, bit_depth: None, chroma: None };
    let mut p = 0usize;
    while p + 2 <= data.len() {
        let id = data[p];
        let len = data[p + 1] as usize;
        let end = p + 2 + len;
        if end > data.len() {
            break;
        }
        // All defined features are single-byte values.
        let v = (len == 1).then(|| data[p + 2]);
        match (id, v) {
            (1, Some(v)) => out.profile = Some(v),
            (2, Some(v)) if v > 0 => out.level = Some(v),
            (3, Some(v)) => out.bit_depth = Some(v),
            (4, Some(v)) => {
                out.chroma = Some(match v {
                    0 | 1 => "4:2:0",
                    2 => "4:2:2",
                    3 => "4:4:4",
                    _ => "?",
                })
            }
            _ => {}
        }
        p = end;
    }
    out
}

/// The report's VP9 profile label: `Profile 2 @ L4.0`, level omitted when the
/// mux didn't state one (levels are stored ×10: 40 → "4.0").
pub fn profile_label(profile: u8, level: Option<u8>) -> String {
    match level {
        Some(l) => format!("Profile {} @ L{}.{}", profile, l / 10, l % 10),
        None => format!("Profile {profile}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a keyframe header bit-by-bit (MSB first) and pad to bytes.
    struct BitWriter {
        bytes: Vec<u8>,
        used: u8,
    }
    impl BitWriter {
        fn new() -> Self {
            BitWriter { bytes: Vec::new(), used: 0 }
        }
        fn push(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                let bit = ((v >> i) & 1) as u8;
                if self.used == 0 {
                    self.bytes.push(0);
                }
                let last = self.bytes.last_mut().unwrap();
                *last |= bit << (7 - self.used);
                self.used = (self.used + 1) % 8;
            }
        }
    }

    /// A keyframe header: profile, optional 10/12 bit, color_space, range,
    /// explicit subsampling for profiles 1/3, and a frame size.
    fn keyframe(profile: u8, ten_or_twelve: u32, cs: u32, full: u32, ss: Option<(u32, u32)>, w: u32, h: u32) -> Vec<u8> {
        let mut b = BitWriter::new();
        b.push(0b10, 2); // frame_marker
        b.push((profile & 1) as u32, 1);
        b.push((profile >> 1) as u32, 1);
        if profile == 3 {
            b.push(0, 1);
        }
        b.push(0, 1); // show_existing_frame
        b.push(0, 1); // frame_type = KEY
        b.push(1, 1); // show_frame
        b.push(0, 1); // error_resilient
        b.push(0x49_8342, 24);
        if profile >= 2 {
            b.push(ten_or_twelve, 1);
        }
        b.push(cs, 3);
        if cs != CS_RGB {
            b.push(full, 1);
            if let Some((x, y)) = ss {
                b.push(x, 1);
                b.push(y, 1);
                b.push(0, 1);
            }
        } else {
            b.push(0, 1); // reserved (profiles 1/3)
        }
        b.push(w - 1, 16);
        b.push(h - 1, 16);
        b.bytes
    }

    #[test]
    fn profile2_hdr_keyframe_parses() {
        // The corpus files' shape: Profile 2, 10-bit, BT.2020, limited, UHD.
        let h = keyframe(2, 0, CS_BT_2020, 0, None, 3840, 2160);
        let f = parse_frame_header(&h).expect("keyframe header");
        assert_eq!(f.profile, 2);
        assert_eq!(f.bit_depth, 10);
        assert_eq!(f.chroma, "4:2:0");
        assert_eq!(f.color.matrix.as_deref(), Some("BT.2020 NCL"));
        assert_eq!(f.color.range.as_deref(), Some("limited"));
        assert!(f.color.transfer.is_none(), "the header names no transfer");
        assert_eq!((f.width, f.height), (3840, 2160));
    }

    #[test]
    fn profile0_and_explicit_subsampling() {
        let h = keyframe(0, 0, CS_BT_709, 0, None, 1920, 1080);
        let f = parse_frame_header(&h).expect("profile 0");
        assert_eq!((f.profile, f.bit_depth, f.chroma), (0, 8, "4:2:0"));
        assert_eq!(f.color.matrix.as_deref(), Some("BT.709"));

        // Profile 1 signals subsampling explicitly: 4:2:2 here.
        let h = keyframe(1, 0, CS_BT_709, 1, Some((1, 0)), 1280, 720);
        let f = parse_frame_header(&h).expect("profile 1");
        assert_eq!((f.profile, f.bit_depth, f.chroma), (1, 8, "4:2:2"));
        assert_eq!(f.color.range.as_deref(), Some("full"));

        // Profile 3, 12-bit, 4:4:4.
        let h = keyframe(3, 1, CS_BT_709, 0, Some((0, 0)), 640, 480);
        let f = parse_frame_header(&h).expect("profile 3");
        assert_eq!((f.profile, f.bit_depth, f.chroma), (3, 12, "4:4:4"));
    }

    #[test]
    fn non_key_and_garbage_are_rejected() {
        // Inter frame (frame_type = 1) carries no colour config.
        let mut b = BitWriter::new();
        b.push(0b10, 2);
        b.push(0, 2); // profile 0
        b.push(0, 1); // show_existing
        b.push(1, 1); // frame_type = INTER
        assert!(parse_frame_header(&b.bytes).is_none());
        // Bad marker / bad sync code / truncation.
        assert!(parse_frame_header(&[0x00, 0x00]).is_none());
        assert!(parse_frame_header(&[]).is_none());
        let mut h = keyframe(2, 0, CS_BT_2020, 0, None, 16, 16);
        h[2] ^= 0xFF; // corrupt the sync code
        assert!(parse_frame_header(&h).is_none());
        h.truncate(3);
        assert!(parse_frame_header(&h).is_none());
    }

    #[test]
    fn webm_codec_private_feature_list() {
        // The corpus webm's CodecPrivate: Profile 2, level 4.0, 10-bit, 4:2:0.
        let cp = [0x01, 0x01, 0x02, 0x02, 0x01, 0x28, 0x03, 0x01, 0x0a, 0x04, 0x01, 0x00];
        let p = parse_webm_codec_private(&cp);
        assert_eq!(p.profile, Some(2));
        assert_eq!(p.level, Some(40));
        assert_eq!(p.bit_depth, Some(10));
        assert_eq!(p.chroma, Some("4:2:0"));
        assert_eq!(profile_label(2, p.level), "Profile 2 @ L4.0");
        assert_eq!(profile_label(2, None), "Profile 2");

        // Empty (the mkv corpus file), truncated, and unknown-id lists.
        let p = parse_webm_codec_private(&[]);
        assert!(p.profile.is_none() && p.bit_depth.is_none());
        let p = parse_webm_codec_private(&[0x01, 0x05, 0x02]); // len overruns
        assert!(p.profile.is_none());
        let p = parse_webm_codec_private(&[0x09, 0x01, 0x07, 0x01, 0x01, 0x02]);
        assert_eq!(p.profile, Some(2), "unknown ids are skipped, not fatal");
    }
}
