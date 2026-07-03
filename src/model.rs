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
pub const SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Serialize)]
pub struct Report {
    /// hdrprobe's own output-schema version (`SCHEMA_VERSION`). The name spells
    /// out whose schema it is: `general.format_version` is the *input's* declared
    /// version (e.g. a DV CM XML's), and `dolby_vision.cm_version` is Dolby's
    /// content-mapping version — this field is neither.
    pub hdrprobe_schema_version: &'static str,
    pub file: String,
    pub size_bytes: u64,
    pub general: General,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr: Option<Hdr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dolby_vision: Option<DolbyVision>,
    /// Present only when HDR10+ metadata was found, mirroring `dolby_vision`:
    /// the object's existence *is* the presence signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr10plus: Option<Hdr10Plus>,
    /// Wall-clock parse time in milliseconds.
    pub elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct General {
    pub container: String,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_profile: Option<String>,
    /// Sidecar schema version, e.g. "4.0.2" from a DV CM XML's root
    /// `<DolbyLabsMDF version=…>` attribute. `None` for video inputs and
    /// sidecars that don't declare one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format_version: Option<String>,
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
pub struct L6 {
    pub max_cll: u16,
    pub max_fall: u16,
    pub max_mastering: u16,
    pub min_mastering: u16,
    /// True when MaxCLL/MaxFALL are both zero (common real defect).
    pub zeroed: bool,
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
            general: General {
                container: "Matroska".to_string(),
                codec: "HEVC".to_string(),
                codec_profile: Some("Main 10, High tier @ L5.1".to_string()),
                format_version: Some("4.0.2".to_string()),
                width: Some(3840),
                height: Some(2160),
                fps: Some(23.976),
                duration_secs: Some(30.0),
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
            },
            hdr: Some(Hdr {
                format: "Dolby Vision + HDR10 (fallback)".to_string(),
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
                bl_present: true,
                el_present: true,
                rpu_present: true,
                el_type: Some("FEL".to_string()),
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
            elapsed_ms: 5.0,
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
            "elapsed_ms",
            "general.container",
            "general.codec",
            "general.codec_profile",
            "general.format_version",
            "general.width",
            "general.height",
            "general.fps",
            "general.duration_secs",
            "general.bitrate.bits_per_sec",
            "general.bitrate.scope",
            "general.bit_depth",
            "general.chroma",
            "general.stereo",
            "general.color.primaries",
            "general.color.transfer",
            "general.color.matrix",
            "general.color.range",
            "hdr.format",
            "hdr.mastering.max_luminance",
            "hdr.mastering.min_luminance",
            "hdr.mastering.primaries",
            "hdr.mastering.primaries_level",
            "hdr.content_light.max_cll",
            "hdr.content_light.max_fall",
            "hdr.content_light.zeroed",
            "dolby_vision.profile",
            "dolby_vision.profile_compat_assumed",
            "dolby_vision.structure",
            "dolby_vision.level",
            "dolby_vision.bl_present",
            "dolby_vision.el_present",
            "dolby_vision.rpu_present",
            "dolby_vision.el_type",
            "dolby_vision.bl_compatibility_id",
            "dolby_vision.compatibility",
            "dolby_vision.cm_version",
            "dolby_vision.l5_active_areas[].width",
            "dolby_vision.l5_active_areas[].height",
            "dolby_vision.l5_active_areas[].left",
            "dolby_vision.l5_active_areas[].right",
            "dolby_vision.l5_active_areas[].top",
            "dolby_vision.l5_active_areas[].bottom",
            "dolby_vision.l5_assumed_canvas[]",
            "dolby_vision.mastering_display.max_luminance",
            "dolby_vision.mastering_display.min_luminance",
            "dolby_vision.mastering_display.primaries",
            "dolby_vision.mastering_display.primaries_level",
            "dolby_vision.fel_brightness_expansion.bl_max_nits",
            "dolby_vision.fel_brightness_expansion.rpu_max_nits",
            "dolby_vision.l6.max_cll",
            "dolby_vision.l6.max_fall",
            "dolby_vision.l6.max_mastering",
            "dolby_vision.l6.min_mastering",
            "dolby_vision.l6.zeroed",
            "dolby_vision.l9_mastering",
            "dolby_vision.l11_content",
            "dolby_vision.l11_white_point",
            "dolby_vision.l11_reference_mode",
            "dolby_vision.trim_targets[].nits",
            "dolby_vision.trim_targets[].levels[]",
            "dolby_vision.rpu_count",
            "dolby_vision.sampled",
            "dolby_vision.census.scene_cuts",
            "dolby_vision.census.dm_version_index",
            "dolby_vision.census.level_presence[].level",
            "dolby_vision.census.level_presence[].rpus_with",
            "hdr10plus.application_version",
            "hdr10plus.num_windows",
            "hdr10plus.profile",
            "hdr10plus.target_max_luminance",
        ];
        expected.sort_unstable();
        assert_eq!(paths, expected, "JSON schema surface changed; see docs/SCHEMA.md");
    }

    #[test]
    fn schema_version_matches_the_documented_one() {
        assert_eq!(SCHEMA_VERSION, "1.0");
        let v = serde_json::to_value(maximal_report()).unwrap();
        assert_eq!(v["hdrprobe_schema_version"], "1.0");
        // The HDR10+ profile char must serialize as a one-character string, as
        // documented, not as a number.
        assert_eq!(v["hdr10plus"]["profile"], "B");
    }
}
