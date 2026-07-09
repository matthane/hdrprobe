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
use crate::prefetch::Frontier;
use crate::progress::Progress;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    Avc,
    Av1,
    Other(String),
}

impl Codec {
    pub fn label(&self) -> String {
        match self {
            Codec::Hevc => "HEVC".to_string(),
            Codec::Avc => "AVC".to_string(),
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
    /// `dv_bl_signal_compatibility_id` (0=none, 1=HDR10, 2=SDR, 4=HLG). `None`
    /// when the record omits it — the compat nibble was added in a later revision,
    /// so the compact 4-byte form used by older Profile-4 TS descriptors has no id.
    pub bl_compatibility_id: Option<u8>,
}

/// File-level demux result: container identity, timing, and the `--full`
/// streaming plans, plus one `TrackDemux` per reported video track.
#[derive(Debug)]
pub struct Demux {
    pub container: &'static str,
    pub duration_secs: Option<f64>,
    /// Report-ordered video tracks, always at least one. MKV orders by
    /// TrackNumber, MP4 by `trak` order, TS by program then PID; the
    /// single-stream backends (raw HEVC/AV1) produce exactly one.
    pub tracks: Vec<TrackDemux>,
    /// TS/M2TS under `--full` only: the plan `sample::scan` uses to stream the
    /// whole video elementary stream in bounded windows (`ts::EsStreamer`)
    /// instead of demux materializing it — the old whole-stream `reassembled`
    /// buffer was the video track's full size (tens of GB for a UHD BD M2TS).
    /// When `Some`, the sampler ignores the tracks' `chunks`/`reassembled`
    /// (they hold only the head metadata window). Every other backend, and the
    /// TS default path, leaves it `None`.
    pub ts_stream: Option<ts::TsFullStream>,
    /// MKV under `--full` only: the plan `sample::scan` uses to walk every
    /// cluster in bounded windows (`mkv::BlockStreamer`), extracting each
    /// window's blocks as they are discovered — index and scan fused into one
    /// pass over the file. When `Some`, the sampler ignores the tracks'
    /// `chunks` (the head metadata window). Every other backend, and the MKV
    /// default path, leaves it `None`.
    pub mkv_stream: Option<mkv::MkvFullStream>,
    /// Raw elementary streams (Annex-B HEVC, AV1 OBU/IVF) under `--full` only:
    /// demux keeps its bounded head walk for metadata and `sample::scan` walks
    /// the whole stream itself, splitting and extracting in one fused pass —
    /// the mirror of `ts_stream`/`mkv_stream`, so the file is read once at any
    /// size instead of an index pass plus a scan pass. When `Some`, the
    /// sampler ignores the track's `chunks` (the head metadata window). Every
    /// other backend, and the raw default paths, leave it `None`.
    pub raw_stream: Option<RawFullStream>,
}

impl Demux {
    /// One-track constructor for the single-stream backends.
    pub fn single(container: &'static str, duration_secs: Option<f64>, track: TrackDemux) -> Demux {
        Demux {
            container,
            duration_secs,
            tracks: vec![track],
            ts_stream: None,
            mkv_stream: None,
            raw_stream: None,
        }
    }
}

/// One reported video track (or logical track: a DV Profile-7 BL+EL pair folds
/// into a single entry — the EL residual is decode-only and never a track of
/// its own in the report).
#[derive(Debug)]
pub struct TrackDemux {
    // The three identity fields are written by the backends now but consumed
    // only by the schema-2.0 report assembly; the allows come off with that
    // change (they exist so the intermediate refactor commits stay
    // warning-free).
    /// Container-native track identity: MKV TrackNumber, MP4 `tkhd` track_ID,
    /// TS primary (BL) PID. `None` where no such id exists (raw streams).
    #[allow(dead_code)]
    pub track_number: Option<u64>,
    /// TS `program_number` of the program this track belongs to; `None` for
    /// every other container.
    #[allow(dead_code)]
    pub program: Option<u16>,
    /// MKV FlagDefault (element 0x88, EBML default true). `None` where the
    /// container has no such flag (MP4/TS/raw).
    #[allow(dead_code)]
    pub default_flag: Option<bool>,
    pub codec: Codec,
    pub nal_format: NalFormat,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    pub bit_depth: Option<u8>,
    pub chroma: Option<String>,
    pub codec_profile: Option<String>,
    /// Stereoscopic/multiview view structure (MP4 `vexu`/`stri`); `None` for
    /// ordinary monoscopic video. Only MV-HEVC (DV Profile 20) sets it today.
    pub stereo: Option<String>,
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
    /// Index into `chunks` of the access unit whose SPS filled the metadata
    /// fields — a random-access point. A TS capture (or a raw ES cut) often
    /// starts mid-GOP: the leading AUs then precede the first IDR, and the
    /// per-GOP prefix SEIs (HLG alt-transfer, ST.2086 mastering, CLL) ride
    /// only RAP AUs, so the sampler must always include this chunk or those
    /// SEIs are silently missed (the head run covers only pre-IDR AUs and the
    /// even spread rarely lands on the few RAPs). `None` when no in-band SPS
    /// was located or the container's chunk 0 is a sync sample by
    /// construction (MP4/MKV).
    pub sps_chunk: Option<usize>,
    /// Owned elementary-stream bytes for containers whose payload is not a
    /// contiguous byte range in the file (TS/M2TS reassembles scattered PES
    /// payloads). When `Some`, `chunks` index into this buffer instead of the
    /// mmap; when `None`, chunks index into the file directly.
    pub reassembled: Option<Vec<u8>>,
}

impl TrackDemux {
    /// Zeroed/`None` track with only the codec identity filled — the
    /// single-track backends set what they parsed and leave the rest.
    pub fn new(codec: Codec, nal_format: NalFormat) -> TrackDemux {
        TrackDemux {
            track_number: None,
            program: None,
            default_flag: None,
            codec,
            nal_format,
            width: 0,
            height: 0,
            fps: None,
            bit_depth: None,
            chroma: None,
            codec_profile: None,
            stereo: None,
            color: ColorInfo::default(),
            dv_config: None,
            dv_dual_track: false,
            mastering: None,
            content_light: None,
            bitrate: None,
            chunks: Vec::new(),
            sps_chunk: None,
            reassembled: None,
        }
    }
}

/// Which raw-stream walk `sample::scan` must drive under `--full`. The walkers
/// themselves live with their formats (`annexb::walk_aus`, `av1::walk_obu_tus`,
/// `av1::walk_ivf_frames`); this only carries what demux already parsed and the
/// walk cannot cheaply rediscover.
#[derive(Debug, Clone, Copy)]
pub enum RawFullStream {
    HevcAnnexB,
    Av1Obu,
    /// `data_start` is the first frame header's offset (past the IVF file
    /// header); `ticks_per_sec` is the header's rate/scale time base, needed to
    /// turn the walk's timestamp span into the stream's true average fps.
    Av1Ivf { data_start: usize, ticks_per_sec: f64 },
}

/// Detect the container type and demux it. `full` requests an exhaustive scan
/// where it changes demux behaviour (TS/M2TS walks the whole stream rather
/// than a bounded head sample). `progress` receives `Phase::Index` ticks from
/// the backends whose `--full` demux walks the whole file; `frontier` rides
/// the same tick sites to keep a remote volume's reads linear. Both are no-op
/// sinks on the default path.
pub fn demux(
    path: &Path,
    data: &[u8],
    full: bool,
    progress: &Progress,
    frontier: &Frontier,
) -> Result<Demux> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let by_ext = match ext.as_str() {
        "mp4" | "m4v" | "mov" | "m4a" => Some(mp4::demux(data)),
        "mkv" | "webm" | "mka" => Some(mkv::demux(data, full)),
        "hevc" | "h265" | "265" | "bin" => Some(annexb::demux(data, full, progress, frontier)),
        "ivf" | "obu" => Some(av1::demux(data, full, progress, frontier)),
        "ts" | "m2ts" | "mts" => Some(ts::demux(data, full, progress, frontier)),
        _ => None,
    };

    // A correctly-named file returns here immediately, so sniffing never runs on the
    // happy path — no latency cost. If the extension-matched backend *failed*, the
    // file may be misnamed (e.g. a TS carrying a .mkv extension): fall through to
    // content sniffing and adopt it only if a sniffed backend actually succeeds,
    // otherwise surface the original, more specific error.
    match by_ext {
        Some(Ok(demux)) => return Ok(demux),
        Some(Err(e)) => {
            if let Some(Ok(demux)) = sniff_demux(data, full, progress, frontier) {
                return Ok(demux);
            }
            return Err(e);
        }
        None => {}
    }

    // Unknown extension: dispatch purely by magic bytes.
    if let Some(res) = sniff_demux(data, full, progress, frontier) {
        return res;
    }

    bail!("unrecognized container (extension '{}')", ext)
}

/// Pick a backend from magic bytes / structural probes alone (extension ignored).
/// `None` when nothing matches.
fn sniff_demux(
    data: &[u8],
    full: bool,
    progress: &Progress,
    frontier: &Frontier,
) -> Option<Result<Demux>> {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return Some(mp4::demux(data));
    }
    if starts_with_ebml(data) {
        return Some(mkv::demux(data, full));
    }
    if av1::is_ivf(data) || av1::is_obu_stream(data) {
        return Some(av1::demux(data, full, progress, frontier));
    }
    if ts::detect_layout(data).is_some() {
        return Some(ts::demux(data, full, progress, frontier));
    }
    if starts_with_start_code(data) {
        return Some(annexb::demux(data, full, progress, frontier));
    }
    None
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

/// Parse a Dolby Vision configuration record (the `dvcC`/`dvvC`/`dvwC` box
/// payload, or the MKV `BlockAddIDExtraData`). `dvwC` is Profile 20 (MV-HEVC)
/// and carries the same record layout. `rec` starts at the `dv_version_major`
/// byte.
pub(crate) fn parse_dovi_config(rec: &[u8]) -> Option<DvConfig> {
    parse_dovi_record(rec, false)
}

/// Parse the MPEG-2 TS `DOVI_video_stream_descriptor` body (tag `0xB0`). It
/// shares the ISOBMFF record's opening fields but, per Table 3-2 of *Dolby
/// Vision Streams Within the MPEG-2 Transport Stream Format*, inserts a
/// `dependency_pid`(13) + reserved(3) block before `dv_bl_signal_compatibility_id`
/// when `bl_present_flag == 0` (the secondary EL/RPU PID of a dual-PID stream).
/// The ISOBMFF `dvcC`/`dvvC` record has no such field. `rec` starts at
/// `dv_version_major`.
pub(crate) fn parse_dovi_ts_descriptor(rec: &[u8]) -> Option<DvConfig> {
    parse_dovi_record(rec, true)
}

fn parse_dovi_record(rec: &[u8], ts_descriptor: bool) -> Option<DvConfig> {
    // Minimum is the 4-byte compact form: major, minor, and a 16-bit bitfield of
    // profile(7)+level(6)+rpu(1)+el(1)+bl(1). The full form adds the compat nibble.
    if rec.len() < 4 {
        return None;
    }
    // rec[0]=major, [1]=minor, then bitfields from byte 2.
    let mut r = BitReader::new(&rec[2..]);
    let profile = r.read_bits(7)? as u8;
    let level = r.read_bits(6)? as u8;
    let rpu_present = r.read_bit()? == 1;
    let el_present = r.read_bit()? == 1;
    let bl_present = r.read_bit()? == 1;
    // TS descriptor only: when the BL is absent (secondary EL/RPU PID), a
    // 16-bit dependency_pid(13)+reserved(3) block precedes the compat nibble.
    // Skip it so the nibble is read from the right offset; if the descriptor is
    // truncated the skip is a no-op and the compat id simply reads as None.
    if ts_descriptor && !bl_present {
        let _ = r.skip_bits(16);
    }
    // `dv_bl_signal_compatibility_id` was added to the record in a later revision;
    // the compact 4-byte form (older Profile-4 TS descriptors) omits it. Read it
    // when present, else leave it unknown rather than guessing 0.
    let bl_compatibility_id = r.read_bits(4).map(|v| v as u8);
    Some(DvConfig {
        profile,
        level: Some(level),
        bl_present,
        el_present,
        rpu_present,
        bl_compatibility_id,
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
    let mut chroma_idc = rec[16] & 0x03;
    let mut bit_depth = (rec[17] & 0x07) + 8;
    let nal_len = (rec[21] & 0x03) + 1;
    // The record's chroma/depth bytes are a summary some muxers zero out even
    // on a Main-10 stream (seen in the wild: a 10-bit MP4 whose hvcC declared
    // 8-bit luma). The embedded SPS is the bitstream's own word — prefer it
    // when it parses, keeping the summary bytes as the fallback.
    if let Some(sps) = crate::hevc::sps::find_sps_in_hvcc(rec).and_then(crate::hevc::sps::parse_sps)
    {
        bit_depth = sps.bit_depth;
        chroma_idc = sps.chroma_format_idc;
    }
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

pub(crate) struct AvccInfo {
    pub bit_depth: u8,
    pub chroma: &'static str,
    pub nal_len: u8,
    pub profile_str: String,
}

/// Parse an AVCDecoderConfigurationRecord (`avcC` payload / MKV CodecPrivate).
/// Unlike `hvcC`, the depth/chroma/profile are not in fixed header fields for
/// every profile, so they come from the embedded SPS.
pub(crate) fn parse_avcc_record(rec: &[u8]) -> Option<AvccInfo> {
    let nal_len = crate::avc::nal::avcc_nal_len(rec)?;
    let sps = crate::avc::sps::parse_sps(crate::avc::nal::find_sps_in_avcc(rec)?)?;
    Some(AvccInfo {
        bit_depth: sps.bit_depth,
        chroma: sps.chroma_str(),
        nal_len,
        profile_str: sps.profile_label(),
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

/// Recover colour info from the SPS embedded in an `avcC` record, for AVC files
/// whose container carries no explicit `colr` box (Profile 9's Rec.709 SDR base
/// signals its VUI here).
pub(crate) fn color_from_avcc(avcc: &[u8]) -> Option<ColorInfo> {
    let info = crate::avc::sps::parse_sps(crate::avc::nal::find_sps_in_avcc(avcc)?)?;
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
        // Dolby's IPT-PQ-c2 colour space, signalled by Profile 20 (MV-HEVC) colr.
        15 => "IPT-PQ-c2",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dvwc_decodes_profile_20() {
        // The `dvwC` payload of a real Profile 20 (MV-HEVC) MP4: dv_version_major=3,
        // minor=0, then profile=20/level=6/rpu=1/el=0/bl=1, compat=0 — matching
        // mediainfo's "Profile 20, dvh1.20.06, BL+RPU". Same record layout as dvcC.
        let rec = [0x03, 0x00, 0x28, 0x35, 0x00];
        let cfg = parse_dovi_config(&rec).expect("valid dvwC record");
        assert_eq!(cfg.profile, 20);
        assert_eq!(cfg.level, Some(6));
        assert!(cfg.rpu_present);
        assert!(!cfg.el_present);
        assert!(cfg.bl_present);
        assert_eq!(cfg.bl_compatibility_id, Some(0));
    }

    #[test]
    fn compact_dovi_config_omits_compat_id() {
        // The 4-byte DV video-stream descriptor of a real Profile-4 TS: major=1,
        // minor=0, then profile=4/level=6/rpu=1/el=1/bl=1 packed in 16 bits with no
        // compatibility nibble — matching mediainfo's "Profile 4, dvhe.04.06,
        // BL+EL+RPU". The EL must survive (else the report reads BL+RPU) and the
        // absent compat id must read as unknown, not a guessed 0.
        let rec = [0x01, 0x00, 0x08, 0x37];
        let cfg = parse_dovi_config(&rec).expect("valid compact record");
        assert_eq!(cfg.profile, 4);
        assert_eq!(cfg.level, Some(6));
        assert!(cfg.rpu_present);
        assert!(cfg.el_present);
        assert!(cfg.bl_present);
        assert_eq!(cfg.bl_compatibility_id, None);
    }

    #[test]
    fn cicp_matrix_names_dolby_ipt() {
        assert_eq!(cicp_matrix(15), Some("IPT-PQ-c2"));
    }

    #[test]
    fn parse_hvcc_prefers_the_embedded_sps_depth() {
        // A real `hvcC` from a 10-bit Main-10 MP4 whose muxer zeroed the
        // record's summary depth bytes (byte 17 declares 8-bit luma). The
        // embedded SPS says 10-bit — the bitstream's own word must win, or the
        // Video line under-reports the depth (MediaInfo agrees on 10).
        let hvcc = [
            0x01, 0x02, 0x20, 0x00, 0x00, 0x00, 0xb0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0xf0,
            0x00, 0xfc, 0xfd, 0xf8, 0xf8, 0x00, 0x00, 0x03, 0x03, 0xa0, 0x00, 0x01, 0x00, 0x23,
            0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00,
            0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x78, 0x11, 0x40, 0xc0, 0x00, 0x00, 0xfa, 0x40,
            0x00, 0x3a, 0x98, 0x20, 0x0f, 0xa6, 0x80, 0xa1, 0x00, 0x01, 0x00, 0x36, 0x42, 0x01,
            0x01, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
            0x00, 0x78, 0xa0, 0x02, 0x80, 0x80, 0x2d, 0x13, 0x65, 0x11, 0x64, 0x91, 0x4a, 0xf0,
            0x10, 0x50, 0x00, 0x00, 0x3e, 0x90, 0x00, 0x0e, 0xa6, 0x08, 0x03, 0xe9, 0xc0, 0x2b,
            0xdc, 0xfc, 0x00, 0x0b, 0x71, 0xa0, 0x00, 0x2d, 0xc6, 0xe4, 0xa2, 0x00, 0x01, 0x00,
            0x07, 0x44, 0x01, 0xc0, 0xac, 0xbe, 0x0e, 0xc9,
        ];
        let h = parse_hvcc_record(&hvcc).expect("valid hvcC");
        assert_eq!(h.bit_depth, 10);
        assert_eq!(h.chroma, "4:2:0");
        assert_eq!(h.profile_str, "Main 10, Main tier @ L4");

        // With the SPS arrays cut off, the summary bytes are the fallback.
        let head_only = &hvcc[..23];
        let h = parse_hvcc_record(head_only).expect("head-only hvcC");
        assert_eq!(h.bit_depth, 8);
    }

    #[test]
    fn parse_avcc_high_profile() {
        // A real `avcC` (AVCDecoderConfigurationRecord) from a Dolby Vision profile
        // 9 MP4: High@L4, 4-byte NAL length prefix, one embedded SPS (1920×1080,
        // 8-bit 4:2:0). Depth/chroma/profile come from that SPS.
        let avcc = [
            0x01, 0x64, 0x00, 0x28, 0xff, 0xe1, 0x00, 0x1d, 0x67, 0x64, 0x00, 0x28, 0xac, 0xb2,
            0x00, 0xf0, 0x04, 0x4f, 0xcb, 0x80, 0xb5, 0x01, 0x01, 0x01, 0x40, 0x00, 0x00, 0x03,
            0x00, 0x40, 0x00, 0x00, 0x0c, 0x03, 0xc6, 0x0c, 0x92, 0x01, 0x00, 0x06, 0x68, 0xeb,
            0xc3, 0xcb, 0x22, 0xc0, 0xfd, 0xf8, 0xf8, 0x00,
        ];
        let a = parse_avcc_record(&avcc).expect("valid avcC");
        assert_eq!(a.bit_depth, 8);
        assert_eq!(a.chroma, "4:2:0");
        assert_eq!(a.nal_len, 4);
        assert_eq!(a.profile_str, "High @ L4");
        // Its embedded SPS also yields the Rec.709 base-layer colour.
        let c = color_from_avcc(&avcc).expect("VUI colour");
        assert_eq!(c.primaries.as_deref(), Some("BT.709"));
        assert_eq!(c.transfer.as_deref(), Some("BT.709"));
        assert_eq!(c.range.as_deref(), Some("limited"));
    }
}
