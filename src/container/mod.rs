//! Container demuxing: produce container-level metadata plus a list of video
//! access-unit byte ranges (`chunks`) to be NAL/OBU-split downstream. All
//! read-only; never decodes pictures.

pub mod annexb;
pub mod av1;
pub mod mkv;
pub mod mp4;
pub mod ts;

use std::path::Path;

use anyhow::{bail, Result};

use crate::bits::BitReader;
use crate::model::{Bitrate, ColorInfo, ContentLight, MasteringDisplay};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    Av1,
    Other(String),
}

impl Codec {
    pub fn label(&self) -> String {
        match self {
            Codec::Hevc => "HEVC".to_string(),
            Codec::Av1 => "AV1".to_string(),
            Codec::Other(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum NalFormat {
    AnnexB,
    /// Length-prefixed NAL units; the field is the length size in bytes.
    LengthPrefixed(u8),
}

/// A video access unit, as an absolute byte range into the file.
#[derive(Debug, Clone, Copy)]
pub struct Chunk {
    pub offset: u64,
    pub size: u64,
}

/// Dolby Vision configuration record from a container box (dvcC/dvvC, etc.).
#[derive(Debug, Clone)]
pub struct DvConfig {
    pub profile: u8,
    pub level: Option<u8>,
    pub bl_present: bool,
    pub el_present: bool,
    pub rpu_present: bool,
    pub bl_compatibility_id: u8,
}

#[derive(Debug)]
pub struct Demux {
    pub container: &'static str,
    pub codec: Codec,
    pub nal_format: NalFormat,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    pub duration_secs: Option<f64>,
    pub bit_depth: Option<u8>,
    pub chroma: Option<String>,
    pub codec_profile: Option<String>,
    pub color: ColorInfo,
    pub dv_config: Option<DvConfig>,
    /// True when the base layer and Dolby Vision enhancement layer are carried on
    /// separate tracks/streams (MP4 dual-`trak`, TS dual-PID) rather than
    /// interleaved in one track. Only meaningful for dual-layer (Profile 7)
    /// content; distinguishes "Dual track, dual layer" from "Single track, dual
    /// layer" in the report. Single-layer backends leave it `false`.
    pub dv_dual_track: bool,
    pub mastering: Option<MasteringDisplay>,
    pub content_light: Option<ContentLight>,
    /// Average bitrate, computed per backend so each container's semantics stay
    /// local: a true per-stream rate where the exact video byte count (or a
    /// stated rate) is known, else a file-length overall rate. `None` without a
    /// usable duration.
    pub bitrate: Option<Bitrate>,
    pub chunks: Vec<Chunk>,
    /// Owned elementary-stream bytes for containers whose payload is not a
    /// contiguous byte range in the file (TS/M2TS reassembles scattered PES
    /// payloads). When `Some`, `chunks` index into this buffer instead of the
    /// mmap; when `None`, chunks index into the file directly.
    pub reassembled: Option<Vec<u8>>,
}

/// Detect the container type and demux it. `full` requests an exhaustive scan
/// where it changes demux behaviour (TS/M2TS reassembles the whole stream rather
/// than a bounded sample).
pub fn demux(path: &Path, data: &[u8], full: bool) -> Result<Demux> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "mp4" | "m4v" | "mov" | "m4a" => return mp4::demux(data),
        "mkv" | "webm" | "mka" => return mkv::demux(data, full),
        "hevc" | "h265" | "265" | "bin" => return annexb::demux(data, full),
        "ivf" | "obu" => return av1::demux(data, full),
        "ts" | "m2ts" | "mts" => return ts::demux(data, full),
        _ => {}
    }

    // Fall back to magic-byte sniffing.
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return mp4::demux(data);
    }
    if starts_with_ebml(data) {
        return mkv::demux(data, full);
    }
    if av1::is_ivf(data) || av1::is_obu_stream(data) {
        return av1::demux(data, full);
    }
    if ts::detect_layout(data).is_some() {
        return ts::demux(data, full);
    }
    if starts_with_start_code(data) {
        return annexb::demux(data, full);
    }

    bail!("unrecognized container (extension '{}')", ext)
}

fn starts_with_start_code(data: &[u8]) -> bool {
    data.len() >= 4
        && ((data[0] == 0 && data[1] == 0 && data[2] == 1)
            || (data[0] == 0 && data[1] == 0 && data[2] == 0 && data[3] == 1))
}

fn starts_with_ebml(data: &[u8]) -> bool {
    // EBML header element ID 0x1A45DFA3.
    data.len() >= 4 && data[0] == 0x1A && data[1] == 0x45 && data[2] == 0xDF && data[3] == 0xA3
}

// --- Shared codec/DV config decoders, used by every container backend. ---

/// Parse a Dolby Vision configuration record (the `dvcC`/`dvvC` box payload, or
/// the MKV `BlockAddIDExtraData`). `rec` starts at the `dv_version_major` byte.
pub(crate) fn parse_dovi_config(rec: &[u8]) -> Option<DvConfig> {
    if rec.len() < 5 {
        return None;
    }
    // rec[0]=major, [1]=minor, then bitfields from byte 2.
    let mut r = BitReader::new(&rec[2..]);
    let profile = r.read_bits(7)? as u8;
    let level = r.read_bits(6)? as u8;
    let rpu_present = r.read_bit()? == 1;
    let el_present = r.read_bit()? == 1;
    let bl_present = r.read_bit()? == 1;
    let compat = r.read_bits(4)? as u8;
    Some(DvConfig {
        profile,
        level: Some(level),
        bl_present,
        el_present,
        rpu_present,
        bl_compatibility_id: compat,
    })
}

pub(crate) struct HvccInfo {
    pub bit_depth: u8,
    pub chroma: &'static str,
    pub nal_len: u8,
    pub profile_str: String,
}

/// Parse an HEVCDecoderConfigurationRecord (`hvcC` payload / MKV CodecPrivate).
pub(crate) fn parse_hvcc_record(rec: &[u8]) -> Option<HvccInfo> {
    if rec.len() < 22 {
        return None;
    }
    // Byte 1: general_profile_space(2) + general_tier_flag(1) + profile_idc(5);
    // byte 12: general_level_idc. Same fields as the SPS profile_tier_level.
    let profile_idc = rec[1] & 0x1F;
    let tier_high = (rec[1] >> 5) & 1 == 1;
    let level_idc = rec[12];
    let chroma_idc = rec[16] & 0x03;
    let bit_depth = (rec[17] & 0x07) + 8;
    let nal_len = (rec[21] & 0x03) + 1;
    let chroma = match chroma_idc {
        0 => "monochrome",
        1 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => "?",
    };
    Some(HvccInfo {
        bit_depth,
        chroma,
        nal_len,
        profile_str: crate::hevc::sps::hevc_profile_label(profile_idc, tier_high, level_idc),
    })
}

/// Parse an AV1CodecConfigurationRecord (`av1C` box payload / MKV AV1
/// CodecPrivate). Returns `(bit_depth, chroma, codec_profile_label)`.
pub(crate) fn parse_av1c_record(rec: &[u8]) -> Option<(u8, &'static str, String)> {
    // byte 0: marker+version; byte 1: seq_profile(3) + seq_level_idx_0(5); byte 2:
    // seq_tier_0(1)+high_bitdepth(1)+twelve_bit(1)+mono(1)+ss_x(1)+ss_y(1)+pos(2).
    if rec.len() < 3 {
        return None;
    }
    let seq_profile = rec[1] >> 5;
    let seq_level_idx = rec[1] & 0x1F;
    let byte2 = rec[2];
    let seq_tier = (byte2 >> 7) & 1;
    let high_bitdepth = (byte2 >> 6) & 1;
    let twelve_bit = (byte2 >> 5) & 1;
    let mono_chrome = (byte2 >> 4) & 1 == 1;
    let ss_x = (byte2 >> 3) & 1;
    let ss_y = (byte2 >> 2) & 1;
    let bit_depth = if twelve_bit == 1 {
        12
    } else if high_bitdepth == 1 {
        10
    } else {
        8
    };
    let chroma = crate::av1::seq::av1_chroma_str(mono_chrome, ss_x, ss_y);
    Some((bit_depth, chroma, crate::av1::seq::av1_profile_label(seq_profile, seq_tier, seq_level_idx)))
}

/// Build a `ColorInfo` from SPS VUI CICP signalling.
pub(crate) fn color_from_vui(vui: &crate::hevc::sps::VuiColor) -> ColorInfo {
    ColorInfo {
        primaries: cicp_primaries(vui.primaries as u16).map(str::to_string),
        transfer: cicp_transfer(vui.transfer as u16).map(str::to_string),
        matrix: cicp_matrix(vui.matrix as u16).map(str::to_string),
        range: Some(if vui.full_range { "full" } else { "limited" }.to_string()),
    }
}

/// Recover colour info from the SPS embedded in an `hvcC` record, for HEVC files
/// whose container carries no explicit colour box/element.
pub(crate) fn color_from_hvcc(hvcc: &[u8]) -> Option<ColorInfo> {
    let sps = crate::hevc::sps::find_sps_in_hvcc(hvcc)?;
    let info = crate::hevc::sps::parse_sps(sps)?;
    info.color.as_ref().map(color_from_vui)
}

pub(crate) fn cicp_primaries(v: u16) -> Option<&'static str> {
    Some(match v {
        1 => "BT.709",
        5 => "BT.601 (PAL)",
        6 => "BT.601 (NTSC)",
        9 => "BT.2020",
        11 => "DCI-P3",
        12 => "Display P3",
        _ => return None,
    })
}
pub(crate) fn cicp_transfer(v: u16) -> Option<&'static str> {
    Some(match v {
        1 => "BT.709",
        6 => "BT.601",
        14 => "BT.2020 (10-bit)",
        15 => "BT.2020 (12-bit)",
        16 => "PQ (SMPTE ST 2084)",
        18 => "HLG (ARIB STD-B67)",
        _ => return None,
    })
}
pub(crate) fn cicp_matrix(v: u16) -> Option<&'static str> {
    Some(match v {
        0 => "RGB",
        1 => "BT.709",
        9 => "BT.2020 NCL",
        10 => "BT.2020 CL",
        _ => return None,
    })
}
