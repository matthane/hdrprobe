//! Container demuxing: produce container-level metadata plus a list of video
//! access-unit byte ranges (`chunks`) to be NAL/OBU-split downstream. All
//! read-only; never decodes pictures.

pub mod annexb;
pub mod av1;
pub mod mkv;
pub mod mp4;
pub mod ts;

use std::path::Path;

use anyhow::{anyhow, bail, Result};

use crate::bits::BitReader;
use crate::model::{Bitrate, ColorInfo, ColorSource, ColorSources, ContentLight, MasteringDisplay};
use crate::prefetch::Frontier;
use crate::progress::Progress;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    Avc,
    Av1,
    Vp9,
    ProRes,
    Other(String),
}

impl Codec {
    pub fn label(&self) -> String {
        match self {
            Codec::Hevc => "HEVC".to_string(),
            Codec::Avc => "AVC".to_string(),
            Codec::Av1 => "AV1".to_string(),
            Codec::Vp9 => "VP9".to_string(),
            Codec::ProRes => "ProRes".to_string(),
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
    /// Container-native track identity: MKV TrackNumber, MP4 `tkhd` track_ID,
    /// TS primary (BL) PID. `None` where no such id exists (raw streams).
    pub track_number: Option<u64>,
    /// TS `program_number` of the program this track belongs to; `None` for
    /// every other container.
    pub program: Option<u16>,
    /// MKV FlagDefault (element 0x88, EBML default true). `None` where the
    /// container has no such flag (MP4/TS/raw).
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
    /// Per-field provenance for `color`, tagged as each backend assembles it in
    /// precedence order (container box/element first, then the coded stream's
    /// own parameter set). The SEI override and the Dolby Vision spec fill are
    /// added later, in `main.rs`, where those inputs exist.
    pub color_source: ColorSources,
    pub dv_config: Option<DvConfig>,
    /// True when the base layer and Dolby Vision enhancement layer are carried on
    /// separate tracks/streams (MP4 dual-`trak`, TS dual-PID) rather than
    /// interleaved in one track. Only meaningful for dual-layer (Profile 7)
    /// content; distinguishes "Dual track, dual layer" from "Single track, dual
    /// layer" in the report. Single-layer backends leave it `false`.
    pub dv_dual_track: bool,
    pub mastering: Option<MasteringDisplay>,
    pub content_light: Option<ContentLight>,
    /// The MP4 `cuvv` box's `cuva_version_map` — HDR Vivid's container
    /// declaration (bit 0 = CUVA v1). `None` everywhere else; only the MP4
    /// backend sets it (no other container defines an HDR Vivid config box).
    pub cuvv_version_map: Option<u16>,
    /// Average bitrate, computed per backend so each container's semantics stay
    /// local: a true per-stream rate where the exact video byte count (or a
    /// stated rate) is known, else a file-length overall rate. `None` without a
    /// usable duration.
    pub bitrate: Option<Bitrate>,
    /// Video access units as byte ranges, in file order.
    ///
    /// **A backend may leave this empty.** `sample::scan` returns early when
    /// every track's list is, so a metadata-only backend (one whose whole
    /// report is served by container headers, with no bitstream side channel to
    /// sample) needs no chunk index, no sampler arm, and no progress or
    /// frontier plumbing at all on the default path. A backend that *can*
    /// cheaply name its first frame's byte range should still fill a one-entry
    /// list, because the codec header parsers run over it.
    ///
    /// That contract stops at the default path. A `--full` walk that has to
    /// read payload the default path never touches (an exact video-stream byte
    /// count, say) still needs its own streaming plan on `Demux`, in the
    /// `ts_stream`/`mkv_stream`/`raw_stream` shape, driven by `sample::scan`
    /// ahead of the empty-chunks early return.
    pub chunks: Vec<Chunk>,
    /// Container-carried ITU-T T.35 payload ranges (absolute file offsets):
    /// MKV `BlockAdditional` payloads with `BlockAddID == 4` — the HDR10+
    /// carriage for VP9 in WebM, whose bitstream has no SEI side channel. The
    /// sampler parses these alongside the chunks (`SeiFindings` merge is
    /// first-wins, so no index alignment with `chunks` is needed). Every other
    /// backend leaves it empty; under `--full` MKV the streaming walk routes
    /// the additions per window instead.
    pub t35_chunks: Vec<Chunk>,
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
            color_source: ColorSources::default(),
            dv_config: None,
            dv_dual_track: false,
            mastering: None,
            content_light: None,
            cuvv_version_map: None,
            bitrate: None,
            chunks: Vec::new(),
            t35_chunks: Vec::new(),
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
    /// IVF frame walk, shared by AV1 and VP9 (the wrapper is codec-agnostic;
    /// extraction dispatches on the track's codec). `data_start` is the first
    /// frame header's offset (past the IVF file header); `ticks_per_sec` is
    /// the header's rate/scale time base, needed to turn the walk's timestamp
    /// span into the stream's true average fps.
    Ivf { data_start: usize, ticks_per_sec: f64 },
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
    match classify_start_code(data) {
        Some(StreamFamily::AnnexB) => Some(annexb::demux(data, full, progress, frontier)),
        // Routed away from the Annex-B backend, which used to claim both and
        // invent metadata from them (see `hevc::nal::emit_nal`), but with no
        // backend of their own yet, so they land on an honest error.
        Some(f) => Some(Err(anyhow!("unsupported container: {}", f.label()))),
        None => None,
    }
}

/// True when `sniff_demux` would route this head to the TS/M2TS backend: the
/// same checks in the same order, so the stdin head budget (sized to
/// `ts::HEAD_SCAN_BYTES` for TS, whose in-band SPS sits ~a GOP in) can never
/// disagree with the backend the sniffer actually picks. Keep the two in sync.
pub(crate) fn sniffs_as_ts(data: &[u8]) -> bool {
    let earlier_check_wins = (data.len() >= 12 && &data[4..8] == b"ftyp")
        || starts_with_ebml(data)
        || av1::is_ivf(data)
        || av1::is_obu_stream(data);
    !earlier_check_wins && ts::detect_layout(data).is_some()
}

/// Which stream family owns a head that begins with an MPEG-style start code.
/// The three-byte prefix `00 00 01` is shared by H.264/H.265 Annex-B, MPEG-1/2
/// and MPEG-4 Part 2 video, and the MPEG-1 system / MPEG-2 program stream
/// layers, so the byte *after* the prefix is what tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFamily {
    /// H.264 / H.265 Annex-B elementary stream.
    AnnexB,
    /// MPEG-1 system stream or MPEG-2 program stream: a pack, a system header,
    /// or (on a mid-file cut) a bare PES packet.
    ProgramStream,
    /// Raw MPEG-1/2 or MPEG-4 Part 2 video elementary stream. Deliberately one
    /// verdict for both, because the byte after the prefix cannot always split
    /// them: `0xB0`/`0xB1`/`0xB6` are Part 2 and reserved in MPEG-2, `0xB7`/
    /// `0xB8` the reverse, but `0xB3` and `0xB5` are genuinely ambiguous, and
    /// resolving those needs a scan for the first unambiguous code. The backend
    /// that parses the stream makes that call.
    MpegVideoEs,
}

impl StreamFamily {
    /// Human-readable name, so every backend that has to decline a stream of
    /// this family declines it in the same words.
    fn label(self) -> &'static str {
        match self {
            StreamFamily::AnnexB => "H.264/H.265 Annex-B elementary stream",
            StreamFamily::ProgramStream => "MPEG program stream (MPEG-1 system / MPEG-2 PS)",
            StreamFamily::MpegVideoEs => "MPEG-1/2 or MPEG-4 Part 2 video elementary stream",
        }
    }
}

/// Route a head beginning with a start code to the family that owns it.
/// `None` when it does not begin with one, or when the bytes contradict the
/// only reading their start code allows.
///
/// The load-bearing rule: **H.264 and H.265 both open their NAL header with
/// `forbidden_zero_bit`, which must be 0 (H.264 §7.3.1, H.265 §7.3.1.2), while
/// every MPEG-1/2, MPEG-4 Part 2 and MPEG system start code has bit 7 set
/// (ISO/IEC 13818-2 Table 6-1, 13818-1 Table 2-18).** So a value `>= 0x80`
/// rules out Annex-B structurally rather than heuristically, and that half of
/// the range is decided, not guessed. Below `0x80` the two spaces genuinely
/// overlap, and the answer is only as good as [`looks_like_nal_header`]; the
/// caveat lives there.
fn classify_start_code(data: &[u8]) -> Option<StreamFamily> {
    // MPEG-1/2 systems and video always write the 3-byte prefix, so the 4-byte
    // form is Annex-B's alone; classifying both through one rule costs nothing
    // and leaves no gap for a stream that leads with a zero byte.
    let sc = if data.starts_with(&[0, 0, 1]) {
        3
    } else if data.starts_with(&[0, 0, 0, 1]) {
        4
    } else {
        return None;
    };
    let value = *data.get(sc)?;
    let next = data.get(sc + 1).copied();
    Some(match value {
        // pack_start_code. The next byte pins the variant: `01xxxxxx` is an
        // MPEG-2 pack (ISO/IEC 13818-1 §2.5.3.4), `0010xxxx` an MPEG-1 one
        // (ISO/IEC 11172-1 §2.4.3.2). Anything else is not a pack header, so
        // these bytes are not a program stream head, and saying nothing beats
        // naming a format the bytes contradict.
        0xBA => match next {
            Some(b) if b & 0xC0 == 0x40 || b & 0xF0 == 0x20 => StreamFamily::ProgramStream,
            _ => return None,
        },
        // program_end, system header, program stream map, or any PES packet:
        // the system layer, or a cut that starts inside one.
        0xB9 | 0xBB..=0xFF => StreamFamily::ProgramStream,
        // Video-layer start codes (`0xB0`..`0xB8`: VOS, sequence header, GOP,
        // extension and so on) plus the high slice codes, since ISO/IEC 13818-2
        // Table 6-1 runs `slice_start_code` from `0x01` all the way to `0xAF`.
        0x80..=0xB8 => StreamFamily::MpegVideoEs,
        // `forbidden_zero_bit` is clear, so this could be a NAL header. MPEG's
        // picture start code (`0x00`) and its low slice codes (`0x01`..`0x7F`)
        // live here too, so validate, and read a failure as MPEG rather than
        // dispatching a backend that would invent metadata.
        _ => {
            if looks_like_nal_header(value, next) {
                StreamFamily::AnnexB
            } else {
                StreamFamily::MpegVideoEs
            }
        }
    })
}

/// Whether `b0` (with `b1`, the byte after it) can open an H.265 or H.264 NAL
/// header. Called only where `forbidden_zero_bit` is already clear.
///
/// **Permissive by construction, and the margin is the point.** The two codecs
/// read the same byte differently and a value need satisfy only one of them, so
/// the AVC reading alone admits every `b0` except `{0x00, 0x20, 0x40, 0x60}`.
/// Measured by simulating a cut at every start code in `mpeg2.m2v`, `mpeg1.m1v`
/// and a retail DVD VOB, 97.8% to 98.8% of an MPEG stream's sub-`0x80` start
/// codes pass here.
///
/// Both readings must stay, and neither may be tightened casually: a VPS-first
/// HEVC stream (`b0 == 0x40`) survives only on the HEVC arm, and an H.264 AUD
/// (`0x09`, whose `b1` is usually `primary_pic_type << 5`) only on the AVC arm,
/// so requiring both would reject both.
///
/// What keeps this honest is that a whole file's first start code is not an
/// arbitrary draw: a raw MPEG video stream opens on a sequence header (`0xB3`)
/// or a VOS (`0xB0`), a program stream on a pack (`0xBA`), and an Annex-B
/// stream on an AUD, VPS or SPS, so the overlap is reachable only by a stream
/// cut mid-picture. For that case `hevc::nal` and `avc::nal` reject
/// `forbidden_zero_bit` a second time while splitting, so a stream slipping
/// through here still reports nothing rather than something invented. Deciding
/// a mid-file cut properly needs a whole-head start-code census, over the
/// thresholds ffmpeg encodes in `libavformat/mpeg.c::mpegps_probe`; that
/// belongs with a program stream backend, not here.
fn looks_like_nal_header(b0: u8, b1: Option<u8>) -> bool {
    // HEVC: forbidden_zero(1) nal_unit_type(6) nuh_layer_id(6)
    // nuh_temporal_id_plus1(3). Types 41..=47 are reserved and never written,
    // and the temporal id is stored plus one, so a zero field is illegal. With
    // no second byte to read the HEVC arm cannot answer, so it declines.
    let hevc_type = (b0 >> 1) & 0x3F;
    let hevc = (hevc_type <= 40 || hevc_type >= 48) && b1.is_some_and(|b| b & 0x07 != 0);
    // AVC: forbidden_zero(1) nal_ref_idc(2) nal_unit_type(5). Type 0 is
    // unspecified and never written.
    let avc = b0 & 0x1F != 0;
    hevc || avc
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
    parse_dovi_record(rec, false).map(|(cfg, _)| cfg)
}

/// Parse the MPEG-2 TS `DOVI_video_stream_descriptor` body (tag `0xB0`). It
/// shares the ISOBMFF record's opening fields but, per Table 3-2 of *Dolby
/// Vision Streams Within the MPEG-2 Transport Stream Format*, inserts a
/// `dependency_pid`(13) + reserved(3) block before `dv_bl_signal_compatibility_id`
/// when `bl_present_flag == 0` (the secondary EL/RPU PID of a dual-PID stream).
/// The ISOBMFF `dvcC`/`dvvC` record has no such field. `rec` starts at
/// `dv_version_major`. The second value is that `dependency_pid` — the base
/// layer's PID, present only in the EL/RPU-PID form — which the TS backend uses
/// to fold the EL into the right track group.
pub(crate) fn parse_dovi_ts_descriptor(rec: &[u8]) -> Option<(DvConfig, Option<u16>)> {
    parse_dovi_record(rec, true)
}

fn parse_dovi_record(rec: &[u8], ts_descriptor: bool) -> Option<(DvConfig, Option<u16>)> {
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
    // dependency_pid(13)+reserved(3) block precedes the compat nibble. Read it
    // — it names the BL PID this EL enhances — so the nibble is read from the
    // right offset; if the descriptor is truncated the read yields None and
    // the compat id simply reads as None too.
    let dependency_pid = if ts_descriptor && !bl_present {
        let pid = r.read_bits(13).map(|v| v as u16);
        let _ = r.skip_bits(3);
        pid
    } else {
        None
    };
    // `dv_bl_signal_compatibility_id` was added to the record in a later revision;
    // the compact 4-byte form (older Profile-4 TS descriptors) omits it. Read it
    // when present, else leave it unknown rather than guessing 0.
    let bl_compatibility_id = r.read_bits(4).map(|v| v as u8);
    Some((
        DvConfig {
            profile,
            level: Some(level),
            bl_present,
            el_present,
            rpu_present,
            bl_compatibility_id,
        },
        dependency_pid,
    ))
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

/// The provenance tag for one decoded CICP field.
///
/// Three outcomes, and the middle one is why this exists: a code that decoded
/// to a name is tagged with its source; a code the source carried but this
/// build cannot name is tagged `UnnamedCode`, which keeps the Dolby Vision spec
/// fill from overwriting a real signal; and "unspecified" (2), or no code at
/// all, is left untagged, which is exactly the state the fill is *for*.
pub(crate) fn cicp_source(code: u16, decoded: Option<&str>, src: ColorSource) -> Option<ColorSource> {
    match decoded {
        Some(_) => Some(src),
        None if code == crate::hevc::sps::UNSPECIFIED_CICP as u16 => None,
        None => Some(ColorSource::UnnamedCode),
    }
}

/// Decode a CICP triplet plus an optional range flag into a colour description
/// and its per-field provenance together. Kept as one step so the raw codes are
/// still in scope when the provenance is decided — `ColorInfo` alone cannot
/// distinguish "unspecified" from "signalled something we have no name for".
pub(crate) fn color_from_cicp(
    primaries: u16,
    transfer: u16,
    matrix: u16,
    full_range: Option<bool>,
    src: ColorSource,
) -> (ColorInfo, ColorSources) {
    let (p, t, m) = (cicp_primaries(primaries), cicp_transfer(transfer), cicp_matrix(matrix));
    let color = ColorInfo {
        primaries: p.map(str::to_string),
        transfer: t.map(str::to_string),
        matrix: m.map(str::to_string),
        range: full_range.map(|f| cicp_range(f).to_string()),
    };
    let sources = ColorSources {
        primaries: cicp_source(primaries, p, src),
        transfer: cicp_source(transfer, t, src),
        matrix: cicp_source(matrix, m, src),
        range: full_range.map(|_| src),
    };
    (color, sources)
}

/// Build a `ColorInfo` and its provenance from SPS VUI CICP signalling. A VUI is
/// always the coded stream's own signalling, so the source is never in doubt.
pub(crate) fn color_from_vui(vui: &crate::hevc::sps::VuiColor) -> (ColorInfo, ColorSources) {
    color_from_cicp(
        vui.primaries as u16,
        vui.transfer as u16,
        vui.matrix as u16,
        Some(vui.full_range),
        ColorSource::Stream,
    )
}

/// Recover colour info from the SPS embedded in an `hvcC` record, for HEVC files
/// whose container carries no explicit colour box/element.
pub(crate) fn color_from_hvcc(hvcc: &[u8]) -> Option<(ColorInfo, ColorSources)> {
    let sps = crate::hevc::sps::find_sps_in_hvcc(hvcc)?;
    let info = crate::hevc::sps::parse_sps(sps)?;
    info.color.as_ref().map(color_from_vui)
}

/// Recover colour info from the SPS embedded in an `avcC` record, for AVC files
/// whose container carries no explicit `colr` box (Profile 9's Rec.709 SDR base
/// signals its VUI here).
pub(crate) fn color_from_avcc(avcc: &[u8]) -> Option<(ColorInfo, ColorSources)> {
    let info = crate::avc::sps::parse_sps(crate::avc::nal::find_sps_in_avcc(avcc)?)?;
    info.color.as_ref().map(color_from_vui)
}

/// Recover colour info from the sequence-header OBU embedded in an `av1C`
/// record's `configOBUs`, for AV1 files whose container carries no explicit
/// colour box/element (mkvmerge leaves AV1 colour in-stream, so an HDR AV1
/// remux commonly has no MKV `Colour` element at all). The record's fixed
/// header is 4 bytes; the OBUs follow, each with a size field except possibly
/// the last — exactly the framing `av1::obu::obus` walks. Mirrors the
/// HEVC/AVC fallbacks above: adopted only when the stream carries an explicit
/// `color_description` (the analogue of the SPS VUI's
/// `colour_description_present_flag`), so a CICP-unspecified stream never
/// overwrites container colour with defaults.
pub(crate) fn color_from_av1c(av1c: &[u8]) -> Option<(ColorInfo, ColorSources)> {
    let config_obus = av1c.get(4..)?;
    let seq = crate::av1::obu::obus(config_obus)
        .find(|o| o.obu_type == crate::av1::obu::OBU_SEQUENCE_HEADER)?;
    let info = crate::av1::seq::parse_sequence_header(seq.payload)?;
    info.color_description_present.then_some(info.color)
}

/// Fill a ProRes track's config/colour gaps from the first frame's header
/// (every ProRes frame is intra-coded and carries one, so the first parseable
/// chunk suffices; both carriage forms parse — the MOV/MP4 sample keeps the
/// `icpf` atom, the Matroska block strips it). Depth/chroma fill only when the
/// container stated none (MKV always; MOV/MP4 derive them from the sample-entry
/// FourCC first). The header's CICP primaries/transfer/matrix fill only when
/// the container signalled none of the three — all-or-nothing, so container
/// authority is never mixed with header bytes (the corpus MKV's header says
/// unspecified under real BT.2020/PQ container signalling, while an
/// ffmpeg-written MOV carries no `colr` box and *only* the header CICP — both
/// directions need this rule). The header has no range field, so range is
/// never touched.
pub(crate) fn fill_prores_stream_fields(track: &mut TrackDemux, data: &[u8]) {
    let missing_cfg = track.bit_depth.is_none() || track.chroma.is_none();
    let signalled_nothing = track.color.primaries.is_none()
        && track.color.transfer.is_none()
        && track.color.matrix.is_none();
    if !missing_cfg && !signalled_nothing {
        return;
    }
    let f = track.chunks.iter().take(32).find_map(|c| {
        let s = c.offset as usize;
        let e = ((c.offset + c.size) as usize).min(data.len());
        (s < e).then(|| crate::prores::parse_frame_header(&data[s..e])).flatten()
    });
    let Some(f) = f else { return };
    if track.bit_depth.is_none() {
        track.bit_depth = Some(f.bit_depth);
    }
    if track.chroma.is_none() {
        track.chroma = Some(f.chroma.to_string());
    }
    if signalled_nothing {
        // Field by field, and never `range`: the frame header has none, so a
        // container-supplied range must keep its own value and provenance.
        let (fc, fs) = f.color;
        track.color.primaries = fc.primaries;
        track.color.transfer = fc.transfer;
        track.color.matrix = fc.matrix;
        track.color_source.primaries = fs.primaries;
        track.color_source.transfer = fs.transfer;
        track.color_source.matrix = fs.matrix;
    }
}

/// ITU-T H.273 `colour_primaries`. Every code the standard defines is named:
/// an unnamed code is indistinguishable in `ColorInfo` from an unsignalled one,
/// which is a distinction the report should not have to make often. 2 stays
/// unnamed on purpose — it *is* "unspecified" — as do the reserved values.
pub(crate) fn cicp_primaries(v: u16) -> Option<&'static str> {
    Some(match v {
        1 => "BT.709",
        4 => "BT.470M",
        5 => "BT.601 (PAL)",
        6 => "BT.601 (NTSC)",
        7 => "SMPTE 240M",
        8 => "Film",
        9 => "BT.2020",
        10 => "XYZ (SMPTE ST 428-1)",
        11 => "DCI-P3",
        12 => "Display P3",
        22 => "EBU 3213-E",
        _ => return None,
    })
}
/// ITU-T H.273 `transfer_characteristics`, named on the same principle as
/// [`cicp_primaries`]. Note that no name here may contain "PQ" or "HLG" unless
/// the curve really is one: `hdr::assemble` classifies on exactly that substring.
pub(crate) fn cicp_transfer(v: u16) -> Option<&'static str> {
    Some(match v {
        1 => "BT.709",
        4 => "Gamma 2.2",
        5 => "Gamma 2.8",
        6 => "BT.601",
        7 => "SMPTE 240M",
        8 => "Linear",
        9 => "Log (100:1)",
        10 => "Log (316:1)",
        11 => "xvYCC (IEC 61966-2-4)",
        12 => "BT.1361",
        13 => "sRGB (IEC 61966-2-1)",
        14 => "BT.2020 (10-bit)",
        15 => "BT.2020 (12-bit)",
        16 => "PQ (SMPTE ST 2084)",
        17 => "SMPTE ST 428-1",
        18 => "HLG (ARIB STD-B67)",
        _ => return None,
    })
}
/// The `video_full_range_flag` label. Not a CICP code point, but it rides the
/// same five-part VUI tuple as the three that are, and the DV spec tables in
/// `dv::ccid` compare against it, so it shares their one decoder namespace.
pub(crate) fn cicp_range(full_range: bool) -> &'static str {
    if full_range {
        "full"
    } else {
        "limited"
    }
}
/// The name of CICP matrix coefficient 15, Dolby's IPT-PQ-C2 colour space.
///
/// Spelled once because it is compared as a live value, not only printed: the
/// Color line names this matrix and no other, and the DV spec tables in
/// `dv::ccid` match against it. SMPTE ST 2128:2023 capitalizes the C, as does
/// every mention across both revisions of Dolby's Profiles and Levels spec.
pub(crate) const IPT_PQ_C2: &str = "IPT-PQ-C2";

/// ITU-T H.273 `matrix_coefficients`, named on the same principle as
/// [`cicp_primaries`]. Codes 5 and 6 carry identical coefficients and differ
/// only in the document defining them, so they take the same practical names as
/// the matching primaries.
pub(crate) fn cicp_matrix(v: u16) -> Option<&'static str> {
    Some(match v {
        0 => "RGB",
        1 => "BT.709",
        4 => "FCC",
        5 => "BT.601 (PAL)",
        6 => "BT.601 (NTSC)",
        7 => "SMPTE 240M",
        8 => "YCgCo",
        9 => "BT.2020 NCL",
        10 => "BT.2020 CL",
        11 => "SMPTE ST 2085",
        12 => "Chroma-derived NCL",
        13 => "Chroma-derived CL",
        14 => "ICtCp",
        // Dolby's IPT-PQ-C2 colour space, signalled by Profile 20 (MV-HEVC) colr.
        15 => IPT_PQ_C2,
        16 => "YCgCo-Re",
        17 => "YCgCo-Ro",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal head that sync-locks as TS: 5 sync bytes at the given stride,
    /// zeros elsewhere (zeros fail every other sniff check).
    fn ts_head(stride: usize) -> Vec<u8> {
        let mut d = vec![0u8; 4 * stride + 1];
        for k in 0..5 {
            d[k * stride] = 0x47;
        }
        d
    }

    #[test]
    fn sniffs_as_ts_locks_both_strides_and_nothing_else() {
        assert!(sniffs_as_ts(&ts_head(188)));
        assert!(sniffs_as_ts(&ts_head(192)));

        // The other sniffable formats must classify away from the TS budget.
        let mut ftyp = vec![0u8; 1024];
        ftyp[4..8].copy_from_slice(b"ftyp");
        assert!(!sniffs_as_ts(&ftyp));
        let mut ebml = vec![0u8; 1024];
        ebml[..4].copy_from_slice(&[0x1A, 0x45, 0xDF, 0xA3]);
        assert!(!sniffs_as_ts(&ebml));
        let mut ivf = vec![0u8; 1024];
        ivf[..4].copy_from_slice(b"DKIF");
        assert!(!sniffs_as_ts(&ivf));

        assert!(!sniffs_as_ts(&[]));
        assert!(!sniffs_as_ts(&[0u8; 1024]));
    }

    #[test]
    fn start_code_discriminator_routes_on_the_byte_after_the_prefix() {
        use StreamFamily::{AnnexB, MpegVideoEs, ProgramStream};

        // Annex-B: an HEVC VPS (type 32 => 0x40, temporal id 1) behind the
        // 4-byte prefix, and an AVC SPS (0x67) behind the 3-byte one.
        assert_eq!(classify_start_code(&[0, 0, 0, 1, 0x40, 0x01, 0xFF]), Some(AnnexB));
        assert_eq!(classify_start_code(&[0, 0, 1, 0x67, 0x42, 0xC0]), Some(AnnexB));

        // Program stream: an MPEG-2 pack (`01xxxxxx`), an MPEG-1 pack
        // (`0010xxxx`), and a bare video PES packet from a mid-file cut.
        assert_eq!(classify_start_code(&[0, 0, 1, 0xBA, 0x44, 0x00]), Some(ProgramStream));
        assert_eq!(classify_start_code(&[0, 0, 1, 0xBA, 0x21, 0x00]), Some(ProgramStream));
        assert_eq!(classify_start_code(&[0, 0, 1, 0xE0, 0x00, 0x08]), Some(ProgramStream));
        // `0xBA` whose next byte fits neither pack form is not a pack at all.
        assert_eq!(classify_start_code(&[0, 0, 1, 0xBA, 0x99, 0x00]), None);

        // Raw MPEG video: an MPEG-1/2 sequence header and a Part 2 VOS. A
        // zero-padded MPEG head reads as a 4-byte start code and must land here
        // too, which is why both prefix lengths run the same ladder.
        assert_eq!(classify_start_code(&[0, 0, 1, 0xB3, 0x02, 0xD0]), Some(MpegVideoEs));
        assert_eq!(classify_start_code(&[0, 0, 1, 0xB0, 0xF5]), Some(MpegVideoEs));
        assert_eq!(classify_start_code(&[0, 0, 0, 1, 0xB3, 0x02, 0xD0]), Some(MpegVideoEs));

        // Not a start code, or too short to read the value byte.
        assert_eq!(classify_start_code(&[0x47, 0, 0, 1]), None);
        assert_eq!(classify_start_code(&[0, 0, 1]), None);
        assert_eq!(classify_start_code(&[]), None);
    }

    #[test]
    fn an_mpeg_picture_header_fails_the_nal_plausibility_checks() {
        // `00 00 01 00` opens an MPEG picture header, so bit 7, which is
        // H.264/H.265's `forbidden_zero_bit`, is clear and the structural rule
        // does not apply. The plausibility checks are what catch this one: AVC
        // nal_unit_type 0 is unspecified, and the HEVC reading needs a nonzero
        // `nuh_temporal_id_plus1`, which a picture header whose
        // `temporal_reference` is 0 does not supply. These exact bytes are
        // `testfiles/sdr/mpeg2.m2v[30..38]`.
        //
        // This pins one byte pattern, not the whole sub-`0x80` range:
        // `nal_header_plausible` is documented as permissive, and a picture
        // header with `temporal_reference % 32 >= 4` does reach the Annex-B
        // backend. The second line of defence for that case is the
        // `forbidden_zero_bit` rejection in `hevc::nal` / `avc::nal`.
        let picture = [0, 0, 1, 0x00, 0x00, 0x0F, 0xFF, 0xF8];
        assert_eq!(classify_start_code(&picture), Some(StreamFamily::MpegVideoEs));
        assert!(!looks_like_nal_header(0x00, Some(0x00)));

        // And the sniffer turns that into an error, never a report.
        let sniffed = sniff_demux(&picture, false, &Progress::off(), &Frontier::off());
        assert!(matches!(sniffed, Some(Err(_))));
    }

    #[test]
    fn sniffer_errors_on_mpeg_heads_and_still_dispatches_annexb() {
        // A program stream pack head and a raw MPEG-2 elementary stream head
        // both used to be demuxed as `raw HEVC (Annex-B)`. The messages are the
        // whole user-visible product of the routing, so pin them, not just the
        // fact that some error came back.
        for (head, want) in [
            ([0, 0, 1, 0xBA, 0x44, 0x00, 0x04, 0x00].as_slice(), "MPEG program stream"),
            ([0, 0, 1, 0xB3, 0x02, 0xD0, 0x21, 0x00].as_slice(), "video elementary stream"),
        ] {
            match sniff_demux(head, false, &Progress::off(), &Frontier::off()) {
                Some(Err(e)) => assert!(
                    e.to_string().contains(want),
                    "expected an error naming {want}, got {e}"
                ),
                other => panic!("MPEG head must not produce a report: {other:?}"),
            }
        }

        // A genuine Annex-B head still dispatches to the raw HEVC backend.
        let hevc = [0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF];
        let r = sniff_demux(&hevc, false, &Progress::off(), &Frontier::off());
        assert!(matches!(&r, Some(Ok(d)) if d.container == "raw HEVC (Annex-B)"));
    }

    #[test]
    fn both_nal_readings_are_load_bearing() {
        // An HEVC VPS-first stream (`0x40`) has AVC nal_unit_type 0 and is
        // admitted only by the HEVC reading; an H.264 AUD (`0x09`, whose next
        // byte is `primary_pic_type << 5`) has a zero HEVC temporal id and is
        // admitted only by the AVC reading. Requiring both would reject both,
        // which is why `looks_like_nal_header` ORs them.
        assert!(looks_like_nal_header(0x40, Some(0x01)));
        assert_eq!(0x40u8 & 0x1F, 0, "the AVC reading alone would reject a VPS");
        assert!(looks_like_nal_header(0x09, Some(0x10)));
        assert_eq!(0x10u8 & 0x07, 0, "the HEVC reading alone would reject an AUD");
    }

    /// Every code ITU-T H.273 defines has a name, cross-checked against
    /// ffmpeg's own enum tables (`ffmpeg -h full`, the `color_primaries`,
    /// `color_trc` and `colorspace` options). An unnamed code is
    /// indistinguishable in `ColorInfo` from an unsignalled one, so the fewer
    /// of them the better.
    #[test]
    fn cicp_tables_name_every_defined_code() {
        for (code, name) in [
            (1u16, "BT.709"),
            (4, "BT.470M"),
            (5, "BT.601 (PAL)"),
            (6, "BT.601 (NTSC)"),
            (7, "SMPTE 240M"),
            (8, "Film"),
            (9, "BT.2020"),
            (10, "XYZ (SMPTE ST 428-1)"),
            (11, "DCI-P3"),
            (12, "Display P3"),
            (22, "EBU 3213-E"),
        ] {
            assert_eq!(cicp_primaries(code), Some(name), "primaries {code}");
        }
        for (code, name) in [
            (1u16, "BT.709"),
            (4, "Gamma 2.2"),
            (5, "Gamma 2.8"),
            (6, "BT.601"),
            (7, "SMPTE 240M"),
            (8, "Linear"),
            (9, "Log (100:1)"),
            (10, "Log (316:1)"),
            (11, "xvYCC (IEC 61966-2-4)"),
            (12, "BT.1361"),
            (13, "sRGB (IEC 61966-2-1)"),
            (14, "BT.2020 (10-bit)"),
            (15, "BT.2020 (12-bit)"),
            (16, "PQ (SMPTE ST 2084)"),
            (17, "SMPTE ST 428-1"),
            (18, "HLG (ARIB STD-B67)"),
        ] {
            assert_eq!(cicp_transfer(code), Some(name), "transfer {code}");
        }
        for (code, name) in [
            (0u16, "RGB"),
            (1, "BT.709"),
            (4, "FCC"),
            (5, "BT.601 (PAL)"),
            (6, "BT.601 (NTSC)"),
            (7, "SMPTE 240M"),
            (8, "YCgCo"),
            (9, "BT.2020 NCL"),
            (10, "BT.2020 CL"),
            (11, "SMPTE ST 2085"),
            (12, "Chroma-derived NCL"),
            (13, "Chroma-derived CL"),
            (14, "ICtCp"),
            (15, "IPT-PQ-C2"),
            (16, "YCgCo-Re"),
            (17, "YCgCo-Ro"),
        ] {
            assert_eq!(cicp_matrix(code), Some(name), "matrix {code}");
        }

        // 2 is "unspecified" and must stay unnamed: `dv::levels` distinguishes
        // it from an unnamed code to decide whether the spec fill may run.
        assert_eq!(cicp_primaries(2), None);
        assert_eq!(cicp_transfer(2), None);
        assert_eq!(cicp_matrix(2), None);
        // Reserved values name nothing either, and never guess.
        for reserved in [0u16, 3, 13, 21, 23, 255] {
            assert_eq!(cicp_primaries(reserved), None, "primaries {reserved} is reserved");
        }
        for reserved in [0u16, 3, 19, 255] {
            assert_eq!(cicp_transfer(reserved), None, "transfer {reserved} is reserved");
        }
        for reserved in [3u16, 18, 255] {
            assert_eq!(cicp_matrix(reserved), None, "matrix {reserved} is reserved");
        }
    }

    /// `hdr::assemble` classifies a base layer by looking for "PQ" and "HLG" as
    /// substrings of the transfer name, so no other curve may contain either.
    #[test]
    fn only_the_pq_and_hlg_curves_carry_those_substrings() {
        for code in 0u16..=255 {
            let Some(name) = cicp_transfer(code) else { continue };
            assert_eq!(
                name.contains("PQ"),
                code == 16,
                "transfer {code} ({name}) must not read as PQ"
            );
            assert_eq!(
                name.contains("HLG"),
                code == 18,
                "transfer {code} ({name}) must not read as HLG"
            );
        }
    }

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
        // Spelled out rather than compared to the constant: this pins the
        // rendered value, per SMPTE ST 2128:2023 and both Dolby revisions.
        assert_eq!(cicp_matrix(15), Some("IPT-PQ-C2"));
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
        let (c, _) = color_from_avcc(&avcc).expect("VUI colour");
        assert_eq!(c.primaries.as_deref(), Some("BT.709"));
        assert_eq!(c.transfer.as_deref(), Some("BT.709"));
        assert_eq!(c.range.as_deref(), Some("limited"));
    }

    #[test]
    fn color_from_av1c_reads_the_embedded_sequence_header() {
        // The `av1C` head of a real DV Profile 10 MKV's CodecPrivate: the 4-byte
        // record header, then the sequence-header OBU whose color_config carries
        // CICP 9/16/9 limited (BT.2020 / PQ). mkvmerge wrote no MKV Colour
        // element for that file — this OBU is the only colour signal it has, and
        // missing it misclassified the DV base (no "HDR10" base tag).
        let av1c = [
            0x81, 0x0c, 0x4e, 0x00, // marker+version, Main profile L5.0, 10-bit 4:2:0
            0x0a, 0x0f, // OBU header: sequence header, 15-byte payload
            0x00, 0x00, 0x00, 0x62, 0xeb, 0xbf, 0xf2, 0x39, 0xd5, 0xf3, 0xa1, 0x22, 0x01, 0x2a,
            0x80,
        ];
        let (c, _) = color_from_av1c(&av1c).expect("colour description");
        assert_eq!(c.primaries.as_deref(), Some("BT.2020"));
        assert_eq!(c.transfer.as_deref(), Some("PQ (SMPTE ST 2084)"));
        assert_eq!(c.matrix.as_deref(), Some("BT.2020 NCL"));
        assert_eq!(c.range.as_deref(), Some("limited"));

        // The same header with color_description_present cleared (the bits that
        // follow shift up: range=limited, chroma position dropped): the stream
        // declares nothing, so the fallback must yield None — never the CICP
        // "unspecified" defaults overwriting container colour.
        let mut unspecified = av1c;
        unspecified[16] = 0x81;
        assert!(color_from_av1c(&unspecified).is_none());
    }
}
