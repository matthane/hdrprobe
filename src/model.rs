//! Serializable result model. One `Report` per input file; drives both the
//! text renderer and `--json`.

use serde::Serialize;

/// serde `skip_serializing_if` predicate for `bool` fields that default false.
fn is_false(b: &bool) -> bool {
    !*b
}

/// Version of hdrprobe's own JSON output schema, `"<major>.<minor>"`, carried on
/// every `Report` and documented in `docs/SCHEMA.md`. Versioned independently of
/// the crate version so an unchanged value tells consumers their scripts need no
/// update. Bump the minor for additive changes (a new optional field, a new value
/// in an enumerated string set); bump the major for anything that can break a
/// correct consumer (renaming/removing a field, changing a type, unit, presence
/// condition, or the meaning of an existing value). Any bump must update
/// `docs/SCHEMA.md` and the golden shape test below in the same change.
pub const SCHEMA_VERSION: &str = "2.4";

#[derive(Debug, Serialize)]
pub struct Report {
    /// hdrprobe's own output-schema version (`SCHEMA_VERSION`). The name spells
    /// out whose schema it is: `format_version` is the *input's* declared
    /// version (e.g. a DV CM XML's), and `dolby_vision.cm_version` is Dolby's
    /// content-mapping version — this field is neither.
    pub hdrprobe_schema_version: &'static str,
    pub file: String,
    pub size_bytes: u64,
    /// Stdin input (`hdrprobe -`) only: true when the stream exceeded the
    /// head budget and only a leading window was probed. When present,
    /// `size_bytes` is the bytes actually probed (not the source's size) and
    /// facts derived from the payload span rather than a declared header
    /// (TS duration, non-MP4 bitrates) are withheld. File probes, and stdin
    /// streams that ended within the budget, omit it.
    #[serde(skip_serializing_if = "is_false")]
    pub input_truncated: bool,
    pub container: String,
    /// Blu-ray ISO probes only: which BDMV playlist/clip was auto-selected as
    /// the main feature. The report's duration, bitrate, and tracks describe
    /// that clip; `size_bytes` stays the whole image's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bd_iso: Option<BdIso>,
    /// Sidecar schema version, e.g. "4.0.2" from a DV CM XML's root
    /// `<DolbyLabsMDF version=…>` attribute. `None` for video inputs and
    /// sidecars that don't declare one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// One entry per video track, always at least one. A single-track file —
    /// the overwhelming majority — has exactly one; an MKV/MP4 carrying
    /// independent video tracks (e.g. a color and a black-and-white cut) or a
    /// multi-program TS has one per track. A Dolby Vision Profile-7 BL+EL pair
    /// is one *logical* track, never two entries. Metadata sidecars carry one
    /// entry too (empty `codec`), so consumers always iterate the array.
    pub video_tracks: Vec<VideoTrack>,
    /// Wall-clock parse time in milliseconds.
    pub elapsed_ms: f64,
}

/// The BDMV main feature a Blu-ray ISO probe selected (see `Report::bd_iso`).
#[derive(Debug, Serialize)]
pub struct BdIso {
    /// Selected playlist file name, e.g. `"00800.mpls"`: the longest by
    /// deduped edit duration.
    pub playlist: String,
    /// The playlist's own edit duration. Distinct from the report's
    /// `duration_secs`, which is the probed clip's transport-clock duration.
    pub playlist_duration_secs: f64,
    /// Probed clip file name under `BDMV/STREAM`, e.g. `"00055.m2ts"`: the
    /// playlist's largest clip.
    pub clip: String,
    /// 1-based position of the probed clip among the playlist's distinct
    /// clips in playback order.
    pub clip_index: usize,
    pub clip_count: usize,
}

#[derive(Debug, Serialize)]
pub struct VideoTrack {
    /// Container-native track identity: MKV TrackNumber, MP4 `tkhd` track_ID,
    /// TS the base layer's PID. Absent where no such id exists (raw elementary
    /// streams, sidecars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u64>,
    /// TS `program_number`, present only for a multi-program mux.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<u16>,
    /// MKV FlagDefault (absent for containers without such a flag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<Bitrate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chroma: Option<String>,
    /// Stereoscopic/multiview view structure, e.g. "Stereoscopic 3D (2 views)",
    /// from the MP4 `vexu`/`stri` boxes of MV-HEVC (DV Profile 20). `None` for
    /// ordinary monoscopic video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stereo: Option<String>,
    pub color: ColorInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr: Option<Hdr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dolby_vision: Option<DolbyVision>,
    /// Present only when HDR10+ metadata was found, mirroring `dolby_vision`:
    /// the object's existence *is* the presence signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr10plus: Option<Hdr10Plus>,
    /// Present only when an SL-HDR information SEI was found, same convention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sl_hdr: Option<SlHdr>,
    /// Present only when HDR Vivid metadata was found, same convention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr_vivid: Option<HdrVivid>,
}

/// Average bitrate. `scope` says whether it's the exact video-stream rate (from a
/// known encoded byte count) or the container's overall rate (file length ÷
/// duration, which also counts audio and packet overhead).
#[derive(Debug, Serialize, Clone, Copy)]
pub struct Bitrate {
    pub bits_per_sec: f64,
    pub scope: BitrateScope,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BitrateScope {
    VideoStream,
    Overall,
}

impl Bitrate {
    /// Exact per-stream rate the container states directly (e.g. the MKV `BPS`
    /// statistics tag), used verbatim — it already reflects the video track's own
    /// duration, which a whole-file duration would only approximate.
    pub fn video_stream_bps(bits_per_sec: f64) -> Self {
        Bitrate { bits_per_sec, scope: BitrateScope::VideoStream }
    }

    /// Per-stream rate from an exact encoded byte count over the stream duration.
    /// Zero bytes means "no sample index", not a real rate — `None`, never 0 b/s.
    pub fn video_stream(bytes: u64, duration_secs: Option<f64>) -> Option<Self> {
        if bytes == 0 {
            return None;
        }
        let d = duration_secs.filter(|d| *d > 0.0)?;
        Some(Bitrate { bits_per_sec: bytes as f64 * 8.0 / d, scope: BitrateScope::VideoStream })
    }

    /// Whole-container rate from the file length; counts audio and packet
    /// overhead, so it is labelled distinctly from a true per-stream rate.
    pub fn overall(file_size: u64, duration_secs: Option<f64>) -> Option<Self> {
        let d = duration_secs.filter(|d| *d > 0.0)?;
        Some(Bitrate { bits_per_sec: file_size as f64 * 8.0 / d, scope: BitrateScope::Overall })
    }
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct ColorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primaries: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matrix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Hdr {
    /// Classified format string, e.g. "Dolby Vision / HDR10 (fallback)".
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mastering: Option<MasteringDisplay>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_light: Option<ContentLight>,
}

#[derive(Debug, Serialize, Clone)]
pub struct MasteringDisplay {
    /// cd/m² (nits).
    pub max_luminance: f64,
    pub min_luminance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primaries: Option<String>,
    /// The Dolby metadata level the `primaries` name came from, when it has
    /// one: 9 for an RPU L9 block, 0 for a DV XML's Level-0 global
    /// `<MasteringDisplay>` chromaticities. `None` for container/SEI-derived
    /// primaries (MDCV, ST.2086), which need no provenance tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primaries_level: Option<u8>,
}

#[derive(Debug, Serialize, Clone, Copy)]
pub struct ContentLight {
    pub max_cll: u16,
    pub max_fall: u16,
    /// True when MaxCLL/MaxFALL are both zero (common real defect).
    pub zeroed: bool,
}

impl ContentLight {
    pub fn new(max_cll: u16, max_fall: u16) -> Self {
        ContentLight { max_cll, max_fall, zeroed: max_cll == 0 && max_fall == 0 }
    }
}

#[derive(Debug, Serialize)]
pub struct DolbyVision {
    /// `profile.compatibility`, e.g. "8.1", "7.6 (FEL)", "5.0", "10.4".
    pub profile: String,
    /// True when the compatibility minor digit was supplied by convention rather
    /// than read from data — i.e. no container dvcC/dvvC and no XML-declared
    /// profile carried the `dv_bl_signal_compatibility_id` (a raw RPU bin, or a
    /// legacy Profile-4 mux whose compact descriptor omits the nibble). The
    /// major number is still RPU-derived; only the `.1`/`.2` is a default.
    #[serde(skip_serializing_if = "is_false")]
    pub profile_compat_assumed: bool,
    /// Layer/track layout, present only for dual-layer (Profile 7) content:
    /// "Single track, dual layer" (BL+EL interleaved in one track/stream) or
    /// "Dual track, dual layer" (BL and EL on separate tracks/PIDs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    /// True when `level` was derived from the coded stream's resolution and
    /// frame rate against the Dolby P&L level table rather than declared by a
    /// container config. Authentic disc muxes carry no declaration at all (a
    /// UHD-BD M2TS signals DV via the playlist STN table, not the PMT), so
    /// without the derivation the field would simply be absent there. The
    /// derived value is a pixel-rate floor: the level's bitrate/tier axis is
    /// not probed, and a declared `dvcC`/`dvvC`/descriptor level always wins.
    #[serde(skip_serializing_if = "is_false")]
    pub level_derived: bool,
    pub bl_present: bool,
    pub el_present: bool,
    pub rpu_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub el_type: Option<String>,
    /// True when the RPU carries the dual-layer composer payload (the NLQ
    /// block whose MEL/FEL fingerprint is `el_type`) but the carriage
    /// demonstrably has no enhancement layer: an explicit dvcC/dvvC/descriptor
    /// with `el_present == 0`, or AV1 with no config (AV1 DV carriage is
    /// single-layer by construction). The classic producer is a custom
    /// transcode that injected a UHD-BD Profile 7 RPU without converting it
    /// (dovi_tool `--mode 2`); the stray payload is inert for playback but
    /// misleads tools that fingerprint the RPU to guess a profile (mkvmerge
    /// derives an AV1 dvvC's compat id that way, yielding out-of-spec "10.6").
    /// A provenance observation, not an error claim. Never fires for a
    /// metadata sidecar (no carriage to compare) or a config-less raw HEVC
    /// stream (its EL may legitimately ride in-band).
    #[serde(skip_serializing_if = "is_false")]
    pub unconverted_dual_layer_rpu: bool,
    /// The composer's reconstructed signal bit depth, read verbatim from the
    /// RPU header's `vdr_bit_depth` field (never derived from the profile:
    /// Profile 7 signals 12, but Profile 4 signals 14). Present only for FEL
    /// streams — the one case where a real residual reconstructs beyond the
    /// 10-bit base layer (BL and EL depths are libdovi-validated to 10-bit on
    /// every parsed RPU). MEL and single-layer RPUs *signal* a 12-bit value
    /// too, but with no (or an empty) residual it describes composer
    /// arithmetic precision, not content depth, so it is withheld there
    /// rather than misread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconstructed_bit_depth: Option<u8>,
    /// BL compatibility id from dvcC/dvvC (0,1,2,4,...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bl_compatibility_id: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    /// "CM v2.9" / "CM v4.0" from L254.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cm_version: Option<String>,
    /// Distinct L5 active areas seen across samples (sampled, may be incomplete).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub l5_active_areas: Vec<ActiveArea>,
    /// When L5 offsets were computed against an *assumed* canvas — a DV XML
    /// carries only aspect ratios, no pixel resolution — this is the `[width,
    /// height]` we assumed. `None` for real bitstreams, whose L5 offsets are
    /// baked into the RPU in actual pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l5_assumed_canvas: Option<[u32; 2]>,
    /// The DV grade's own mastering-display luminance: the RPU DM header's
    /// `source_min_pq`/`source_max_pq` (or, for a DV CM XML, the exact global
    /// Level-0 values). Distinct from the HDR section's mastering line, which
    /// describes the *base layer* (container/ST.2086 SEI) — on a Profile 7
    /// title the DV grade can exceed it (4000-nit grade over a 1000-nit
    /// HDR10 base).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mastering_display: Option<MasteringDisplay>,
    /// Metadata indication that the FEL likely expands brightness beyond the
    /// base layer: the DV grade's own mastering display (`source_max_pq`) is
    /// meaningfully brighter than the base layer's declared one (container
    /// MDCV / ST.2086 SEI), the classic case being a 4000-nit grade over a
    /// 1000-nit HDR10 base. Only set
    /// for FEL video inputs: a MEL's residual is empty (it can never carry
    /// brightness the BL lacks), and a metadata sidecar has no base layer to
    /// expand beyond. Metadata tier only; confirming actual pixel expansion
    /// needs a decode, which hdrprobe never does, so absence is not proof.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fel_brightness_expansion: Option<FelBrightnessExpansion>,
    /// Metadata indication that the DV grade's mastering gamut (a recognized
    /// L9) disagrees with the base layer's own declared mastering primaries (a
    /// *signalled* container MDCV box or ST.2086 SEI, never a fallback), e.g.
    /// a BT.2020-claiming MDCV over a DCI-P3 D65 L9 left behind by a re-encode.
    /// Both labels come from the same gamut matcher, so plain inequality is the
    /// verdict; unrecognized coordinates on either side never fire it. Video
    /// inputs only: a metadata sidecar has no base layer to disagree with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mastering_primaries_mismatch: Option<MasteringPrimariesMismatch>,
    /// The RPU's L6 block: MaxCLL/MaxFALL plus the mastering luminances, in
    /// the bitstream's raw integer units.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l6: Option<L6>,
    /// L9 mastering-display color space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l9_mastering: Option<String>,
    /// L11 content type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l11_content: Option<String>,
    /// L11 intended white point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l11_white_point: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l11_reference_mode: Option<bool>,
    /// Distinct trim targets: the L2/L8 union across the read RPUs plus any
    /// L10-defined target displays (custom L8 targets, folded into the L8
    /// set), each tagged with the level(s) that produced it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trim_targets: Vec<TrimTarget>,
    /// Number of RPUs successfully parsed.
    pub rpu_count: usize,
    /// True when the report reflects sampling rather than a full scan.
    pub sampled: bool,
    /// Authoring cadence of the dynamic metadata, decided by comparing
    /// consecutive frames' DM payloads. Present only when every frame's RPU
    /// was read in stream order — a `--full` video scan or a DV sidecar; a
    /// sampled video run has no adjacent frames to compare, so it carries
    /// no verdict rather than a guessed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_cadence: Option<MetadataCadence>,
    /// Exhaustive per-level census, present only under `--full`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub census: Option<DvCensus>,
}

/// The metadata-cadence verdict plus the consecutive-frame evidence behind it.
/// `frame_pairs` counts the adjacent-frame DM comparisons made (frames − 1 on
/// an exhaustive read) and `changed_pairs` how many differed, so a consumer
/// can see how decisive the majority verdict was: a shot-based title changes
/// at its scene cuts only (a few percent), per-frame analysis at well over
/// half.
#[derive(Debug, Serialize)]
pub struct MetadataCadence {
    /// `"per-shot"` (frames within a shot share one DM payload — the standard
    /// CM authoring workflow) or `"per-frame"` (each frame carries its own).
    pub cadence: String,
    /// Consecutive-frame DM payload comparisons made.
    pub frame_pairs: usize,
    /// How many of those comparisons differed.
    pub changed_pairs: usize,
}

/// Exhaustive metadata census over every RPU in the title (`--full`).
#[derive(Debug, Serialize)]
pub struct DvCensus {
    /// RPUs carrying a scene-cut (`scene_refresh_flag`) — i.e. shot count.
    pub scene_cuts: usize,
    /// DM version index from L254 (`dm_version_index`), if L254 present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dm_version_index: Option<u8>,
    /// Per-level presence: how many RPUs carried each metadata level.
    pub level_presence: Vec<LevelPresence>,
}

#[derive(Debug, Serialize)]
pub struct LevelPresence {
    pub level: u8,
    pub rpus_with: usize,
}

/// The evidence pair behind the FEL brightness-expansion flag, both in nits:
/// the base layer's declared mastering max and the RPU grade's mastering max.
#[derive(Debug, Serialize, Clone, Copy)]
pub struct FelBrightnessExpansion {
    pub bl_max_nits: f64,
    pub rpu_max_nits: f64,
}

/// The evidence pair behind the mastering-primaries-mismatch flag: the base
/// layer's declared mastering gamut and the DV grade's L9 gamut, both as the
/// shared matcher's label names.
#[derive(Debug, Serialize, Clone)]
pub struct MasteringPrimariesMismatch {
    pub bl_primaries: String,
    pub rpu_primaries: String,
}

/// One distinct trim target, in nits, plus the level(s) that produced it — 2
/// and/or 8. The 8 covers both read L8 trims and target displays defined by
/// the title's global L10 metadata: a display index is a CM v4.0 (L8)
/// mechanism by construction, so an L10-defined display is a custom L8 target
/// even when its per-shot trims sit outside the sample. L10 itself is never
/// listed — it is bitstream plumbing, not a trim level.
#[derive(Debug, Serialize)]
pub struct TrimTarget {
    pub nits: u32,
    pub levels: Vec<u8>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
pub struct ActiveArea {
    pub width: u32,
    pub height: u32,
    /// Aspect ratio numerator:denominator presentation string, e.g. "2.39:1".
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
}

#[derive(Debug, Serialize, Clone, Copy)]
pub struct L6 {
    pub max_cll: u16,
    pub max_fall: u16,
    pub max_mastering: u16,
    pub min_mastering: u16,
    /// True when MaxCLL/MaxFALL are both zero (common real defect).
    pub zeroed: bool,
}

/// SL-HDR (ETSI TS 103 433) reconstruction metadata, title-stable header
/// facts only — the per-picture reconstruction parameters are never reported.
#[derive(Debug, Serialize)]
pub struct SlHdr {
    /// SL-HDR mode: 1 (SDR base), 2 (PQ base), 3 (HLG base). Also the digit
    /// in the classified `hdr.format` component ("SL-HDR2").
    pub mode: u8,
    /// Declared TS 103 433 spec version, "major.minor" (e.g. "1.0").
    pub spec_version: String,
    /// "parameter-based" or "table-based". Absent when the SEI carried the
    /// cancel flag or a reserved payload-mode value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_mode: Option<String>,
    /// The target picture the adaptation metadata is tuned toward, when the
    /// SEI carries the block: named CICP primaries and max luminance (cd/m²).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_primaries: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_max_luminance: Option<u32>,
    /// The source mastering display carried inside the SL-HDR metadata
    /// (`src_mdcv`), distinct from the base layer's own MDCV signalling.
    /// `primaries_level` is never set here (it is a DV provenance tag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mastering: Option<MasteringDisplay>,
}

/// HDR Vivid (CUVA, T/UWA 005) metadata, title-stable header facts only —
/// the per-frame tone-mapping payload is never reported.
#[derive(Debug, Serialize)]
pub struct HdrVivid {
    /// CUVA metadata version, "major.minor" (e.g. "1.0"), from the T.35
    /// provider-oriented code — the field the standard itself calls the
    /// version, not the SEI-path number MediaInfo renders (which is the
    /// data-set type below).
    pub version: String,
    /// `system_start_code`, the dynamic-metadata data-set type (1 in every
    /// known stream). Absent when detection came only from the MP4 `cuvv`
    /// declaration and no frame's SEI was read (e.g. `--no-rpu`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_start_code: Option<u8>,
    /// Distinct targeted-system-display max luminances of the tone-mapping
    /// parameter sets, nits (12-bit PQ codes through the standard-target
    /// snap), sorted ascending. Display anchors the per-frame curves are
    /// computed toward — the HDR Vivid analogue of the DV trim-target set,
    /// and like it a sampled union unless the scan read every frame.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub target_max_luminances: Vec<u32>,
    /// True when `target_max_luminances` came from a sampled spread of frames
    /// rather than a full scan, mirroring `dolby_vision.sampled`.
    pub sampled: bool,
}

#[derive(Debug, Serialize)]
pub struct Hdr10Plus {
    pub application_version: u8,
    pub num_windows: u8,
    /// ST.2094-40 profile: 'A' (histogram only) or 'B' (Bézier tone-mapping curve).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<char>,
    /// Target display max luminance the grade was made for (nits).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_max_luminance: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Report` with every optional field populated and every array non-empty,
    /// so serialization exercises the complete schema surface.
    fn maximal_report() -> Report {
        Report {
            hdrprobe_schema_version: SCHEMA_VERSION,
            file: "movie.mkv".to_string(),
            size_bytes: 1,
            input_truncated: true,
            container: "Matroska".to_string(),
            bd_iso: Some(BdIso {
                playlist: "00800.mpls".to_string(),
                playlist_duration_secs: 8065.0,
                clip: "00055.m2ts".to_string(),
                clip_index: 1,
                clip_count: 1,
            }),
            format_version: Some("4.0.2".to_string()),
            duration_secs: Some(30.0),
            video_tracks: vec![maximal_track()],
            elapsed_ms: 5.0,
        }
    }

    fn maximal_track() -> VideoTrack {
        VideoTrack {
            track_number: Some(1),
            program: Some(28),
            default: Some(true),
            codec: "HEVC".to_string(),
            codec_profile: Some("Main 10, High tier @ L5.1".to_string()),
            width: Some(3840),
            height: Some(2160),
            fps: Some(23.976),
            bitrate: Some(Bitrate::video_stream_bps(1.0)),
            bit_depth: Some(10),
            chroma: Some("4:2:0".to_string()),
            stereo: Some("Stereoscopic 3D (2 views)".to_string()),
            color: ColorInfo {
                primaries: Some("BT.2020".to_string()),
                transfer: Some("PQ (SMPTE ST 2084)".to_string()),
                matrix: Some("BT.2020 NCL".to_string()),
                range: Some("limited".to_string()),
            },
            hdr: Some(Hdr {
                format: "Dolby Vision / HDR10 (fallback)".to_string(),
                mastering: Some(MasteringDisplay {
                    max_luminance: 1000.0,
                    min_luminance: 0.0001,
                    primaries: Some("DCI-P3 D65".to_string()),
                    primaries_level: Some(9),
                }),
                content_light: Some(ContentLight::new(737, 130)),
            }),
            dolby_vision: Some(DolbyVision {
                profile: "7.6 (FEL)".to_string(),
                profile_compat_assumed: true,
                structure: Some("Single track, dual layer".to_string()),
                level: Some(6),
                level_derived: true,
                bl_present: true,
                el_present: true,
                rpu_present: true,
                el_type: Some("FEL".to_string()),
                unconverted_dual_layer_rpu: true,
                reconstructed_bit_depth: Some(12),
                bl_compatibility_id: Some(6),
                compatibility: Some("HDR10-compatible".to_string()),
                cm_version: Some("CM v4.0".to_string()),
                l5_active_areas: vec![ActiveArea {
                    width: 3840,
                    height: 1608,
                    left: 0,
                    right: 0,
                    top: 276,
                    bottom: 276,
                }],
                l5_assumed_canvas: Some([3840, 2160]),
                mastering_display: Some(MasteringDisplay {
                    max_luminance: 4000.0,
                    min_luminance: 0.0001,
                    primaries: Some("BT.2020".to_string()),
                    primaries_level: Some(0),
                }),
                fel_brightness_expansion: Some(FelBrightnessExpansion {
                    bl_max_nits: 1000.0,
                    rpu_max_nits: 4000.0,
                }),
                mastering_primaries_mismatch: Some(MasteringPrimariesMismatch {
                    bl_primaries: "BT.2020".to_string(),
                    rpu_primaries: "DCI-P3 D65".to_string(),
                }),
                l6: Some(L6 {
                    max_cll: 737,
                    max_fall: 130,
                    max_mastering: 1000,
                    min_mastering: 1,
                    zeroed: false,
                }),
                l9_mastering: Some("BT.2020".to_string()),
                l11_content: Some("Movies".to_string()),
                l11_white_point: Some("D65".to_string()),
                l11_reference_mode: Some(true),
                trim_targets: vec![TrimTarget { nits: 100, levels: vec![2, 8] }],
                rpu_count: 722,
                sampled: false,
                metadata_cadence: Some(MetadataCadence {
                    cadence: "per-shot".to_string(),
                    frame_pairs: 721,
                    changed_pairs: 4,
                }),
                census: Some(DvCensus {
                    scene_cuts: 5,
                    dm_version_index: Some(2),
                    level_presence: vec![LevelPresence { level: 1, rpus_with: 722 }],
                }),
            }),
            hdr10plus: Some(Hdr10Plus {
                application_version: 1,
                num_windows: 1,
                profile: Some('B'),
                target_max_luminance: Some(400),
            }),
            sl_hdr: Some(SlHdr {
                mode: 2,
                spec_version: "1.0".to_string(),
                payload_mode: Some("parameter-based".to_string()),
                target_primaries: Some("BT.2020".to_string()),
                target_max_luminance: Some(100),
                source_mastering: Some(MasteringDisplay {
                    max_luminance: 1000.0,
                    min_luminance: 0.0001,
                    primaries: Some("BT.2020".to_string()),
                    primaries_level: None,
                }),
            }),
            hdr_vivid: Some(HdrVivid {
                version: "1.0".to_string(),
                system_start_code: Some(1),
                target_max_luminances: vec![100, 500],
                sampled: true,
            }),
        }
    }

    /// Flatten a serialized value into `a.b.c` / `a[].b` leaf paths.
    fn collect_paths(v: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    let p =
                        if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    collect_paths(val, &p, out);
                }
            }
            serde_json::Value::Array(items) => match items.first() {
                Some(first) => collect_paths(first, &format!("{prefix}[]"), out),
                None => out.push(format!("{prefix}[]")),
            },
            _ => out.push(prefix.to_string()),
        }
    }

    /// Golden test pinning the serialized schema surface. If this fails, the JSON
    /// output shape changed: update `docs/SCHEMA.md`, decide whether the change is
    /// additive (bump `SCHEMA_VERSION`'s minor) or breaking (bump its major), and
    /// only then update the expected list here.
    #[test]
    fn schema_shape_is_pinned() {
        let v = serde_json::to_value(maximal_report()).expect("report serializes");
        let mut paths = Vec::new();
        collect_paths(&v, "", &mut paths);
        paths.sort();

        let mut expected = vec![
            "hdrprobe_schema_version",
            "file",
            "size_bytes",
            "input_truncated",
            "container",
            "bd_iso.playlist",
            "bd_iso.playlist_duration_secs",
            "bd_iso.clip",
            "bd_iso.clip_index",
            "bd_iso.clip_count",
            "format_version",
            "duration_secs",
            "elapsed_ms",
            "video_tracks[].track_number",
            "video_tracks[].program",
            "video_tracks[].default",
            "video_tracks[].codec",
            "video_tracks[].codec_profile",
            "video_tracks[].width",
            "video_tracks[].height",
            "video_tracks[].fps",
            "video_tracks[].bitrate.bits_per_sec",
            "video_tracks[].bitrate.scope",
            "video_tracks[].bit_depth",
            "video_tracks[].chroma",
            "video_tracks[].stereo",
            "video_tracks[].color.primaries",
            "video_tracks[].color.transfer",
            "video_tracks[].color.matrix",
            "video_tracks[].color.range",
            "video_tracks[].hdr.format",
            "video_tracks[].hdr.mastering.max_luminance",
            "video_tracks[].hdr.mastering.min_luminance",
            "video_tracks[].hdr.mastering.primaries",
            "video_tracks[].hdr.mastering.primaries_level",
            "video_tracks[].hdr.content_light.max_cll",
            "video_tracks[].hdr.content_light.max_fall",
            "video_tracks[].hdr.content_light.zeroed",
            "video_tracks[].dolby_vision.profile",
            "video_tracks[].dolby_vision.profile_compat_assumed",
            "video_tracks[].dolby_vision.structure",
            "video_tracks[].dolby_vision.level",
            "video_tracks[].dolby_vision.level_derived",
            "video_tracks[].dolby_vision.bl_present",
            "video_tracks[].dolby_vision.el_present",
            "video_tracks[].dolby_vision.rpu_present",
            "video_tracks[].dolby_vision.el_type",
            "video_tracks[].dolby_vision.unconverted_dual_layer_rpu",
            "video_tracks[].dolby_vision.reconstructed_bit_depth",
            "video_tracks[].dolby_vision.bl_compatibility_id",
            "video_tracks[].dolby_vision.compatibility",
            "video_tracks[].dolby_vision.cm_version",
            "video_tracks[].dolby_vision.l5_active_areas[].width",
            "video_tracks[].dolby_vision.l5_active_areas[].height",
            "video_tracks[].dolby_vision.l5_active_areas[].left",
            "video_tracks[].dolby_vision.l5_active_areas[].right",
            "video_tracks[].dolby_vision.l5_active_areas[].top",
            "video_tracks[].dolby_vision.l5_active_areas[].bottom",
            "video_tracks[].dolby_vision.l5_assumed_canvas[]",
            "video_tracks[].dolby_vision.mastering_display.max_luminance",
            "video_tracks[].dolby_vision.mastering_display.min_luminance",
            "video_tracks[].dolby_vision.mastering_display.primaries",
            "video_tracks[].dolby_vision.mastering_display.primaries_level",
            "video_tracks[].dolby_vision.fel_brightness_expansion.bl_max_nits",
            "video_tracks[].dolby_vision.fel_brightness_expansion.rpu_max_nits",
            "video_tracks[].dolby_vision.mastering_primaries_mismatch.bl_primaries",
            "video_tracks[].dolby_vision.mastering_primaries_mismatch.rpu_primaries",
            "video_tracks[].dolby_vision.l6.max_cll",
            "video_tracks[].dolby_vision.l6.max_fall",
            "video_tracks[].dolby_vision.l6.max_mastering",
            "video_tracks[].dolby_vision.l6.min_mastering",
            "video_tracks[].dolby_vision.l6.zeroed",
            "video_tracks[].dolby_vision.l9_mastering",
            "video_tracks[].dolby_vision.l11_content",
            "video_tracks[].dolby_vision.l11_white_point",
            "video_tracks[].dolby_vision.l11_reference_mode",
            "video_tracks[].dolby_vision.trim_targets[].nits",
            "video_tracks[].dolby_vision.trim_targets[].levels[]",
            "video_tracks[].dolby_vision.rpu_count",
            "video_tracks[].dolby_vision.sampled",
            "video_tracks[].dolby_vision.metadata_cadence.cadence",
            "video_tracks[].dolby_vision.metadata_cadence.frame_pairs",
            "video_tracks[].dolby_vision.metadata_cadence.changed_pairs",
            "video_tracks[].dolby_vision.census.scene_cuts",
            "video_tracks[].dolby_vision.census.dm_version_index",
            "video_tracks[].dolby_vision.census.level_presence[].level",
            "video_tracks[].dolby_vision.census.level_presence[].rpus_with",
            "video_tracks[].hdr10plus.application_version",
            "video_tracks[].hdr10plus.num_windows",
            "video_tracks[].hdr10plus.profile",
            "video_tracks[].hdr10plus.target_max_luminance",
            "video_tracks[].sl_hdr.mode",
            "video_tracks[].sl_hdr.spec_version",
            "video_tracks[].sl_hdr.payload_mode",
            "video_tracks[].sl_hdr.target_primaries",
            "video_tracks[].sl_hdr.target_max_luminance",
            "video_tracks[].sl_hdr.source_mastering.max_luminance",
            "video_tracks[].sl_hdr.source_mastering.min_luminance",
            "video_tracks[].sl_hdr.source_mastering.primaries",
            "video_tracks[].hdr_vivid.version",
            "video_tracks[].hdr_vivid.system_start_code",
            "video_tracks[].hdr_vivid.target_max_luminances[]",
            "video_tracks[].hdr_vivid.sampled",
        ];
        expected.sort_unstable();
        assert_eq!(paths, expected, "JSON schema surface changed; see docs/SCHEMA.md");
    }

    #[test]
    fn schema_version_matches_the_documented_one() {
        assert_eq!(SCHEMA_VERSION, "2.4");
        let v = serde_json::to_value(maximal_report()).unwrap();
        assert_eq!(v["hdrprobe_schema_version"], "2.4");
        // The HDR10+ profile char must serialize as a one-character string, as
        // documented, not as a number.
        assert_eq!(v["video_tracks"][0]["hdr10plus"]["profile"], "B");
    }

    #[test]
    fn schema_doc_header_matches_schema_version() {
        let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join("SCHEMA.md");
        let doc = std::fs::read_to_string(doc).expect("docs/SCHEMA.md must be readable");
        let header = format!("**Schema version: {SCHEMA_VERSION}**");
        assert!(
            doc.contains(&header),
            "docs/SCHEMA.md header does not state schema version {SCHEMA_VERSION}"
        );
        let history_entry = format!("- **{SCHEMA_VERSION}**:");
        assert!(
            doc.contains(&history_entry),
            "docs/SCHEMA.md version history has no {SCHEMA_VERSION} entry"
        );
    }
}
