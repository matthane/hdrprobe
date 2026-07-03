//! Serializable result model. One `Report` per input file; drives both the
//! text renderer and `--json`.

use serde::Serialize;

/// serde `skip_serializing_if` predicate for `bool` fields that default false.
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub file: String,
    pub size_bytes: u64,
    pub general: General,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr: Option<Hdr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dolby_vision: Option<DolbyVision>,
    pub hdr10plus: Hdr10Plus,
    /// Wall-clock parse time in milliseconds.
    pub elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct General {
    pub container: String,
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
    pub duration_secs: Option<f64>,
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
    pub fn video_stream(bytes: u64, duration_secs: Option<f64>) -> Option<Self> {
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
    /// Classified format string, e.g. "Dolby Vision + HDR10 (fallback)".
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
    pub bl_present: bool,
    pub el_present: bool,
    pub rpu_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub el_type: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l6_fallback: Option<L6Fallback>,
    /// L9 mastering-display color space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l9_mastering: Option<String>,
    /// L11 content type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l11_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l11_reference_mode: Option<bool>,
    /// Distinct L2/L8 trim targets (union across samples), each tagged with the
    /// level(s) that produced it so provenance is per-value rather than aggregate.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trim_targets: Vec<TrimTarget>,
    /// Number of RPUs successfully parsed.
    pub rpu_count: usize,
    /// True when the report reflects sampling rather than a full scan.
    pub sampled: bool,
    /// Exhaustive per-level census, present only under `--full`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub census: Option<DvCensus>,
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

/// One distinct trim target, in nits, plus the level(s) that produced it — 2
/// and/or 8. (L8's target display may be *defined* by L10, but the trim itself
/// is still L8, so L10 is never listed here.)
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
pub struct L6Fallback {
    pub max_cll: u16,
    pub max_fall: u16,
    pub max_mastering: u16,
    pub min_mastering: u16,
    /// True when MaxCLL/MaxFALL are both zero (common real defect).
    pub zeroed: bool,
}

#[derive(Debug, Serialize)]
pub struct Hdr10Plus {
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_version: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_windows: Option<u8>,
    /// ST.2094-40 profile: 'A' (histogram only) or 'B' (Bézier tone-mapping curve).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<char>,
    /// Target display max luminance the grade was made for (nits).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_max_luminance: Option<u32>,
}

impl Hdr10Plus {
    /// The "not present" value, used when a report carries no HDR10+ metadata.
    pub fn absent() -> Self {
        Hdr10Plus {
            present: false,
            application_version: None,
            num_windows: None,
            profile: None,
            target_max_luminance: None,
        }
    }
}
