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
pub const SCHEMA_VERSION: &str = "3.0";

#[derive(Debug, Serialize)]
pub struct Report {
    /// hdrprobe's own output-schema version (`SCHEMA_VERSION`). The name spells
    /// out whose schema it is: `format_version` is the *input's* declared
    /// version (e.g. a DV CM XML's), and `dolby_vision.cm_version` is Dolby's
    /// content-mapping version — this field is neither.
    pub hdrprobe_schema_version: &'static str,
    pub file: String,
    pub size_bytes: u64,
    /// True when only part of the input was (or could be) probed. Two cases
    /// share the flag. **Stdin** (`hdrprobe -`): the stream exceeded the head
    /// budget, only a leading window was probed, `size_bytes` is the bytes
    /// actually probed, and facts derived from the payload span rather than a
    /// declared header (TS duration, non-MP4 bitrates) are withheld. **File
    /// probes** (open-items B6): the container itself declares more bytes
    /// than the file holds (AVI RIFF segment sizes, ASF `File Properties`,
    /// FLV `onMetaData.filesize`) — a partial download or a capture that
    /// never closed; the backend has already withheld what a prefix cannot
    /// support, and this names why. Absent for whole files and for stdin
    /// streams that ended within the budget.
    #[serde(skip_serializing_if = "is_false")]
    pub input_truncated: bool,
    pub container: String,
    /// Blu-ray ISO probes only: which BDMV playlist/clip was auto-selected as
    /// the main feature. The report's duration, bitrate, and tracks describe
    /// that clip; `size_bytes` stays the whole image's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bd_iso: Option<BdIso>,
    /// DVD-Video ISO probes only: which title set was auto-selected as the
    /// main feature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dvd_iso: Option<DvdIso>,
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
}

/// The VIDEO_TS main feature a DVD-Video ISO probe selected (see
/// `Report::dvd_iso`): the byte-largest title set. The report's duration,
/// bitrate, and tracks describe that set's VOB program stream; `size_bytes`
/// stays the whole image's.
#[derive(Debug, Serialize)]
pub struct DvdIso {
    /// Title set number: the probed VOBs are `VTS_<vts>_1.VOB` onward.
    pub vts: u16,
    /// Title VOBs in the probed set (the ≤1 GiB slices of one program
    /// stream; menu VOBs are excluded).
    pub vob_count: usize,
    /// The longest program chain's declared playback time from the set's
    /// IFO — the feature's authored runtime, the analogue of a Blu-ray
    /// playlist's edit duration. When present it is also the report's
    /// `duration_secs` (the declared runtime is the DVD duration authority;
    /// the program-stream PTS span is only the fallback, being blind to
    /// cell/layer-break resets between its windows). Absent when the IFO is
    /// missing or unparseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_duration_secs: Option<f64>,
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
    /// Resolved codec display name ("HEVC"), or the container's identifier
    /// verbatim for a codec this build has no parser for. Absent for metadata
    /// sidecars, which carry no video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// The container's own codec identifier, verbatim: the MP4/MOV
    /// sample-entry FourCC (`"hvc1"` vs `"hev1"`, post-encryption recovery),
    /// the Matroska CodecID (with the inner VfW FourCC appended for
    /// `V_MS/VFW/FOURCC`), a TS PMT `stream_type` in hex (`"0x24"`), an
    /// AVI/ASF FourCC (hex form when unprintable), an FLV legacy id
    /// (`"7"`) or Enhanced FourCC, RealMedia's VIDO FourCC, or an Ogg
    /// mapping name (`"theora"`). Absent where no container-level identifier
    /// exists: raw elementary streams, program streams (whose PES id is
    /// already `track_number`), raw DV, and sidecars.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_id: Option<String>,
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
    /// Pixel (sample) aspect ratio: the width of one pixel over its height,
    /// 1.0 being square. Signalled by the coded stream or the container, or
    /// derived exactly from a signalled display ratio and the coded size.
    /// Absent when nothing signals either ratio; never a guessed square.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_aspect_ratio: Option<f64>,
    /// Display aspect ratio, width over height of the presented picture.
    /// Signalled directly (MPEG-2's DAR codes, MKV display size, AVI `vprp`)
    /// or derived exactly from the pixel aspect ratio and the coded size.
    /// Present exactly when `pixel_aspect_ratio` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_aspect_ratio: Option<f64>,
    /// `"progressive"` or `"interlaced"`, from a sequence-level signal of the
    /// coded stream. Absent when the format has no such signal or the stream
    /// states none — absence is "unsignalled", never "progressive".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_type: Option<String>,
    /// Stereoscopic/multiview view structure, e.g. "Stereoscopic 3D (2 views)",
    /// from the MP4 `vexu`/`stri` boxes of MV-HEVC (DV Profile 20). `None` for
    /// ordinary monoscopic video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stereo: Option<String>,
    pub color: ColorInfo,
    /// Where each `color` field came from, same field order.
    pub color_source: ColorSources,
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

/// Average bitrate. `scope` says whether it's the video-stream rate or the
/// container's overall rate (file length ÷ duration, which also counts audio
/// and packet overhead); `source` says whether hdrprobe computed the number or
/// read it from a header.
#[derive(Debug, Serialize, Clone, Copy)]
pub struct Bitrate {
    pub bits_per_sec: f64,
    pub scope: BitrateScope,
    pub source: BitrateSource,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BitrateScope {
    VideoStream,
    Overall,
}

/// How a `Bitrate` was obtained. `Measured` = computed from per-sample /
/// per-chunk sums or actual payload/file bytes (MP4 `stsz`, an AVI index, a
/// `--full` streamed sum, every `overall` rate). `Declared` = a single rate
/// or byte count stated in a container header, however the muxer obtained it
/// (MKV `BPS`/`NUMBER_OF_BYTES` statistics tags, ASF `Data Bitrate`, FLV
/// `videodatarate`, RealMedia's MDPR average); a declared value can differ
/// from the encoded reality by a percent or two.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BitrateSource {
    Measured,
    Declared,
}

impl Bitrate {
    /// Per-stream rate the container states directly (e.g. the MKV `BPS`
    /// statistics tag), used verbatim — it already reflects the video track's own
    /// duration, which a whole-file duration would only approximate.
    pub fn video_stream_bps(bits_per_sec: f64) -> Self {
        Bitrate {
            bits_per_sec,
            scope: BitrateScope::VideoStream,
            source: BitrateSource::Declared,
        }
    }

    /// Per-stream rate from an exact encoded byte count over the stream duration.
    /// Zero bytes means "no sample index", not a real rate — `None`, never 0 b/s.
    pub fn video_stream(bytes: u64, duration_secs: Option<f64>) -> Option<Self> {
        if bytes == 0 {
            return None;
        }
        let d = duration_secs.filter(|d| *d > 0.0)?;
        Some(Bitrate {
            bits_per_sec: bytes as f64 * 8.0 / d,
            scope: BitrateScope::VideoStream,
            source: BitrateSource::Measured,
        })
    }

    /// Whole-container rate from the file length; counts audio and packet
    /// overhead, so it is labelled distinctly from a true per-stream rate.
    pub fn overall(file_size: u64, duration_secs: Option<f64>) -> Option<Self> {
        let d = duration_secs.filter(|d| *d > 0.0)?;
        Some(Bitrate {
            bits_per_sec: file_size as f64 * 8.0 / d,
            scope: BitrateScope::Overall,
            source: BitrateSource::Measured,
        })
    }

    /// Re-tag a rate as header-declared. For the one shape the constructors
    /// don't cover: a quotient of a *declared* byte count over a duration
    /// (MKV `NUMBER_OF_BYTES` without `BPS`), where the numerator's provenance
    /// is the honest tag.
    pub fn declared(mut self) -> Self {
        self.source = BitrateSource::Declared;
        self
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

/// Where one field of `ColorInfo` came from. Per field rather than per object
/// because the two genuinely differ: a Dolby Vision Profile 5 stream signals its
/// range and nothing else, so its range is `Stream` while its primaries,
/// transfer and matrix are `Spec`.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColorSource {
    /// A container colour box or element: MP4 `colr`/`vpcC`, MKV `Colour`.
    Container,
    /// The coded stream's own signalling: an SPS/sequence-header VUI, whether
    /// read in band or from the parameter set embedded in a codec config record
    /// (`hvcC`/`avcC`/`av1C`), or a VP9/ProRes frame header.
    Stream,
    /// An SEI message overriding the above — today only the HLG/PQ
    /// `alternative_transfer_characteristic` message (SEI 147).
    Sei,
    /// Not signalled anywhere: supplied by the Dolby Vision profile and
    /// compatibility id, which define the base layer's colour outright. Only
    /// ever fills a field nothing signalled, and only when the compatibility id
    /// was itself declared or spec-fixed — never when it was inferred from the
    /// very colour this would be filling.
    Spec,
    /// **Internal, never serialized.** The source carried a CICP code for this
    /// field that this build has no name for, so `ColorInfo` leaves it empty —
    /// there is no label to put there — and it is otherwise indistinguishable
    /// from a field nothing signalled at all. Recording it keeps the Dolby
    /// Vision spec fill from overwriting a real signal and then claiming, via
    /// `Spec`, that nothing was signalled. `hidden` below keeps it out of the
    /// report, so `ColorSources` still carries a tag exactly when `ColorInfo`
    /// carries a value.
    UnnamedCode,
}

/// `skip_serializing_if` for every `ColorSources` field: a field with no value
/// has no provenance to report, and `UnnamedCode` marks precisely that case.
fn hidden(v: &Option<ColorSource>) -> bool {
    matches!(v, None | Some(ColorSource::UnnamedCode))
}

/// Per-field provenance for `ColorInfo`, in the same field order. A field is
/// tagged exactly when `ColorInfo` carries a value for it.
#[derive(Debug, Serialize, Default, Clone, Copy)]
pub struct ColorSources {
    #[serde(skip_serializing_if = "hidden")]
    pub primaries: Option<ColorSource>,
    #[serde(skip_serializing_if = "hidden")]
    pub transfer: Option<ColorSource>,
    #[serde(skip_serializing_if = "hidden")]
    pub matrix: Option<ColorSource>,
    #[serde(skip_serializing_if = "hidden")]
    pub range: Option<ColorSource>,
}

/// Every colour producer builds `ColorInfo` and `ColorSources` together through
/// `container::color_from_cicp`, which is the only place that sees the raw CICP
/// codes and so the only place that can tell an unnamed code from an absent one.
/// There is deliberately no constructor here that derives provenance from a
/// finished `ColorInfo`: it could not make that distinction, and a caller using
/// one would silently relabel fields it never wrote.

#[derive(Debug, Serialize)]
pub struct Hdr {
    /// Classified format string, e.g. "Dolby Vision / HDR10".
    pub format: String,
    /// The base signal a decoder without the dynamic-metadata layer receives:
    /// "HDR10", "HLG", or "SDR" — exactly the format string's base tag as a
    /// field of its own. Absent when the stream has no independently viewable
    /// base (DV compatibility id 0: Profiles 5, 10.0 and 20).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The base layer's declared mastering display (container MDCV box or
    /// ST.2086 SEI, with the DV L6 fallback on an HDR10 base). One name with
    /// `dolby_vision.mastering_display` and `sl_hdr.source_mastering_display`,
    /// which describe the same kind of fact from other pipeline stages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mastering_display: Option<MasteringDisplay>,
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

/// Provenance of `DolbyVision::bl_compatibility_id`, in descending order of
/// evidence. Every rung but `Assumed` fills `bl_compatibility_id` and
/// `compatibility`; `Assumed` resolves the *label* only and leaves both fields
/// absent, because a display convention is not a value the stream carries.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompatSource {
    /// Read from a container dvcC/dvvC/TS descriptor, or a DV CM XML's declared
    /// `GenerateProfile`.
    Declared,
    /// Fixed by the profile's own definition — Dolby's profile table pairs
    /// profiles 4, 5, 7 and 9 (and the legacy 0-3, 6) with exactly one id, so no
    /// stream evidence is needed.
    Spec,
    /// Deduced from the base layer's signalled VUI, for a profile whose
    /// definition admits several ids. Returned only when exactly one candidate
    /// survives; an ambiguous signal resolves nothing.
    Inferred,
    /// Convention default with no evidence behind it: a Profile 8 that declares
    /// no id and whose base layer does not separate CCID 1, 2 and 4 is labelled
    /// `8.1` because that is what the ecosystem writes, and this field is how
    /// the report discloses that the digit is not backed by data.
    Assumed,
}

/// How much of the input's frame metadata a report's sampled-union facts rest
/// on. `Sampled`: a spread of frames (the default probe) — union fields may be
/// incomplete. `Full`: every frame was read (`--full`, or a sidecar, which is
/// exhaustive by construction). `None`: no frame metadata was read at all
/// (`--no-rpu`, or a container declaration with no readable frame), so every
/// frame-derived field is absent and the counts are zero.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Sampled,
    Full,
    None,
}

/// Provenance of `DolbyVision::level`, the same shape as `CompatSource`.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LevelSource {
    /// Declared by a container `dvcC`/`dvvC`/TS descriptor.
    Declared,
    /// Derived from the coded stream's resolution and frame rate against the
    /// Dolby P&L level table (a pixel-rate floor; a declared level always wins).
    Derived,
}

#[derive(Debug, Serialize)]
pub struct DolbyVision {
    /// `profile.compatibility`, e.g. "8.1", "7.6 (FEL)", "5.0", "10.4".
    pub profile: String,
    /// Where the base-layer cross-compatibility id behind the profile's minor
    /// digit came from. Absent only when nothing resolved it and the profile has
    /// no convention default either (a bare Profile 10 or 20 whose base layer
    /// signals too little to separate its candidates).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat_source: Option<CompatSource>,
    /// True when the base layer's actual transfer characteristic is Dolby's
    /// proprietary "PQ with reshaping" rather than the plain PQ its VUI names.
    /// Dolby states this for cross-compatibility id 0 outright: a transfer
    /// characteristic of 16 there "generally indicates perceptual quantization
    /// (PQ)", but "the actual proprietary transfer characteristic, even when
    /// signaled with 16, is 'PQ with reshaping'". It has no CICP code point, so
    /// it cannot live in `color.transfer` — that object stays a strict CICP
    /// projection. Video inputs only: a metadata sidecar has no base layer whose
    /// transfer this would describe.
    #[serde(skip_serializing_if = "is_false")]
    pub pq_reshaping: bool,
    /// True when the profile and compatibility id pair into a combination Dolby
    /// has withdrawn: `8.3` or `8.5`, the two rows of Annex I ("Profiles not
    /// supported for new applications") that name a pairing rather than a whole
    /// profile. Profile 8 itself is current, and the legacy *profiles* Annex I
    /// also lists (0, 1, 2, 3, 4, 6) are not flagged — plenty of real content
    /// uses them and "legacy" is not a defect. A provenance observation about
    /// how the stream was authored, not a playability claim.
    #[serde(skip_serializing_if = "is_false")]
    pub deprecated_combination: bool,
    /// Layer/track layout, present only for dual-layer (Profile 7) content:
    /// "Single track, dual layer" (BL+EL interleaved in one track/stream) or
    /// "Dual track, dual layer" (BL and EL on separate tracks/PIDs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    /// Where `level` came from; present exactly when `level` is. `declared`
    /// is a container `dvcC`/`dvvC`/TS descriptor. `derived` means computed
    /// from the coded stream's resolution and frame rate against the Dolby
    /// P&L level table: authentic disc muxes carry no declaration at all (a
    /// UHD-BD M2TS signals DV via the playlist STN table, not the PMT), so
    /// without the derivation the field would simply be absent there. The
    /// derived value is a pixel-rate floor: the level's bitrate/tier axis is
    /// not probed, and a declared level always wins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level_source: Option<LevelSource>,
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
    /// carries only aspect ratios, no pixel resolution — this is the canvas
    /// we assumed. `None` for real bitstreams, whose L5 offsets are baked
    /// into the RPU in actual pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l5_assumed_canvas: Option<AssumedCanvas>,
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
    /// The L11 (Dolby Vision IQ / content type) block, present when L11 was
    /// seen. Its three fields ride one block, so they appear together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l11: Option<L11>,
    /// Distinct trim targets: the L2/L8 union across the read RPUs plus any
    /// L10-defined target displays (custom L8 targets, folded into the L8
    /// set), each tagged with the level(s) that produced it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trim_targets: Vec<TrimTarget>,
    /// Number of RPUs successfully parsed.
    pub rpu_count: usize,
    /// What the DV facts rest on: `sampled` (a spread of RPUs — the union
    /// fields may be incomplete), `full` (every RPU was read: `--full` or a
    /// sidecar), or `none` (no RPU was read: `--no-rpu`, or a container
    /// config whose track yielded none — `rpu_count` is 0 and the section is
    /// built from the config alone).
    pub coverage: Coverage,
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

/// The canvas a sidecar's L5 active-area dimensions were computed against
/// (see `DolbyVision::l5_assumed_canvas`).
#[derive(Debug, Serialize, Clone, Copy)]
pub struct AssumedCanvas {
    pub width: u32,
    pub height: u32,
}

/// The RPU's L11 content-type block. All three fields ride one block, so a
/// present `l11` always carries all of them.
#[derive(Debug, Serialize)]
pub struct L11 {
    /// Content type, named per Dolby's L11 definitions: "Default", "Movies",
    /// "Game", "Sport", "User Generated Content", or "Unknown" for values
    /// outside the published range.
    pub content: String,
    /// Intended white point: "D65" (0, the default), "D93" (8), or "code N"
    /// for codes Dolby accepts but does not publicly name.
    pub white_point: String,
    /// Reference-mode flag.
    pub reference_mode: bool,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
pub struct ActiveArea {
    pub width: u32,
    pub height: u32,
    /// L5 crop offsets, in pixels from each canvas edge.
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
    pub source_mastering_display: Option<MasteringDisplay>,
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
    /// What the HDR Vivid facts rest on, mirroring `dolby_vision.coverage`:
    /// `sampled` (a spread of frames), `full` (every frame read), or `none`
    /// (no frame's SEI was read — a `cuvv` box-only detection, e.g. under
    /// `--no-rpu`, where `version` alone survives).
    pub coverage: Coverage,
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
            dvd_iso: Some(DvdIso { vts: 4, vob_count: 6, title_duration_secs: Some(6547.5) }),
            format_version: Some("4.0.2".to_string()),
            duration_secs: Some(30.0),
            video_tracks: vec![maximal_track()],
        }
    }

    fn maximal_track() -> VideoTrack {
        VideoTrack {
            track_number: Some(1),
            program: Some(28),
            default: Some(true),
            codec: Some("HEVC".to_string()),
            codec_id: Some("hvc1".to_string()),
            codec_profile: Some("Main 10, High tier @ L5.1".to_string()),
            width: Some(3840),
            height: Some(2160),
            fps: Some(23.976),
            bitrate: Some(Bitrate::video_stream_bps(1.0)),
            bit_depth: Some(10),
            chroma: Some("4:2:0".to_string()),
            pixel_aspect_ratio: Some(1.0),
            display_aspect_ratio: Some(16.0 / 9.0),
            scan_type: Some("progressive".to_string()),
            stereo: Some("Stereoscopic 3D (2 views)".to_string()),
            color: ColorInfo {
                primaries: Some("BT.2020".to_string()),
                transfer: Some("PQ (SMPTE ST 2084)".to_string()),
                matrix: Some("BT.2020 NCL".to_string()),
                range: Some("limited".to_string()),
            },
            color_source: ColorSources {
                primaries: Some(ColorSource::Container),
                transfer: Some(ColorSource::Sei),
                matrix: Some(ColorSource::Stream),
                range: Some(ColorSource::Spec),
            },
            hdr: Some(Hdr {
                format: "Dolby Vision / HDR10".to_string(),
                base: Some("HDR10".to_string()),
                mastering_display: Some(MasteringDisplay {
                    max_luminance: 1000.0,
                    min_luminance: 0.0001,
                    primaries: Some("DCI-P3 D65".to_string()),
                    primaries_level: Some(9),
                }),
                content_light: Some(ContentLight::new(737, 130)),
            }),
            dolby_vision: Some(DolbyVision {
                profile: "7.6 (FEL)".to_string(),
                compat_source: Some(CompatSource::Declared),
                pq_reshaping: true,
                deprecated_combination: true,
                structure: Some("Single track, dual layer".to_string()),
                level: Some(6),
                level_source: Some(LevelSource::Derived),
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
                l5_assumed_canvas: Some(AssumedCanvas { width: 3840, height: 2160 }),
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
                l11: Some(L11 {
                    content: "Movies".to_string(),
                    white_point: "D65".to_string(),
                    reference_mode: true,
                }),
                trim_targets: vec![TrimTarget { nits: 100, levels: vec![2, 8] }],
                rpu_count: 722,
                coverage: Coverage::Full,
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
                source_mastering_display: Some(MasteringDisplay {
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
                coverage: Coverage::Sampled,
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
            "dvd_iso.vts",
            "dvd_iso.vob_count",
            "dvd_iso.title_duration_secs",
            "format_version",
            "duration_secs",
            "video_tracks[].track_number",
            "video_tracks[].program",
            "video_tracks[].default",
            "video_tracks[].codec",
            "video_tracks[].codec_id",
            "video_tracks[].codec_profile",
            "video_tracks[].width",
            "video_tracks[].height",
            "video_tracks[].fps",
            "video_tracks[].bitrate.bits_per_sec",
            "video_tracks[].bitrate.scope",
            "video_tracks[].bitrate.source",
            "video_tracks[].bit_depth",
            "video_tracks[].chroma",
            "video_tracks[].pixel_aspect_ratio",
            "video_tracks[].display_aspect_ratio",
            "video_tracks[].scan_type",
            "video_tracks[].stereo",
            "video_tracks[].color.primaries",
            "video_tracks[].color.transfer",
            "video_tracks[].color.matrix",
            "video_tracks[].color.range",
            "video_tracks[].color_source.primaries",
            "video_tracks[].color_source.transfer",
            "video_tracks[].color_source.matrix",
            "video_tracks[].color_source.range",
            "video_tracks[].hdr.format",
            "video_tracks[].hdr.base",
            "video_tracks[].hdr.mastering_display.max_luminance",
            "video_tracks[].hdr.mastering_display.min_luminance",
            "video_tracks[].hdr.mastering_display.primaries",
            "video_tracks[].hdr.mastering_display.primaries_level",
            "video_tracks[].hdr.content_light.max_cll",
            "video_tracks[].hdr.content_light.max_fall",
            "video_tracks[].hdr.content_light.zeroed",
            "video_tracks[].dolby_vision.profile",
            "video_tracks[].dolby_vision.compat_source",
            "video_tracks[].dolby_vision.pq_reshaping",
            "video_tracks[].dolby_vision.deprecated_combination",
            "video_tracks[].dolby_vision.structure",
            "video_tracks[].dolby_vision.level",
            "video_tracks[].dolby_vision.level_source",
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
            "video_tracks[].dolby_vision.l5_assumed_canvas.width",
            "video_tracks[].dolby_vision.l5_assumed_canvas.height",
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
            "video_tracks[].dolby_vision.l11.content",
            "video_tracks[].dolby_vision.l11.white_point",
            "video_tracks[].dolby_vision.l11.reference_mode",
            "video_tracks[].dolby_vision.trim_targets[].nits",
            "video_tracks[].dolby_vision.trim_targets[].levels[]",
            "video_tracks[].dolby_vision.rpu_count",
            "video_tracks[].dolby_vision.coverage",
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
            "video_tracks[].sl_hdr.source_mastering_display.max_luminance",
            "video_tracks[].sl_hdr.source_mastering_display.min_luminance",
            "video_tracks[].sl_hdr.source_mastering_display.primaries",
            "video_tracks[].hdr_vivid.version",
            "video_tracks[].hdr_vivid.system_start_code",
            "video_tracks[].hdr_vivid.target_max_luminances[]",
            "video_tracks[].hdr_vivid.coverage",
        ];
        expected.sort_unstable();
        assert_eq!(paths, expected, "JSON schema surface changed; see docs/SCHEMA.md");
    }

    /// `UnnamedCode` is internal bookkeeping, not a reported provenance: it
    /// marks a field `ColorInfo` has no value for, and a field with no value has
    /// nothing to attribute. It must never reach the output, or it would break
    /// the documented guarantee that `color` and `color_source` carry the same
    /// key set.
    #[test]
    fn the_unnamed_code_marker_never_serializes() {
        let sources = ColorSources {
            primaries: Some(ColorSource::Container),
            transfer: Some(ColorSource::UnnamedCode),
            matrix: Some(ColorSource::UnnamedCode),
            range: Some(ColorSource::Stream),
        };
        let v = serde_json::to_value(sources).expect("serializes");
        let obj = v.as_object().expect("object");
        assert_eq!(obj.len(), 2, "only the two reportable fields survive: {v}");
        assert_eq!(obj["primaries"], "container");
        assert_eq!(obj["range"], "stream");
        assert!(!v.to_string().contains("unnamed"), "the marker leaked: {v}");
    }

    /// The measured/declared split is baked into the constructors: a stated
    /// per-stream rate is `declared`, computed quotients are `measured`, and
    /// `declared()` re-tags the one shape outside that mapping (a declared
    /// byte count over a duration). If a constructor's tag changes, every
    /// backend's provenance changes with it — this pins the mapping.
    #[test]
    fn bitrate_source_is_baked_into_the_constructors() {
        assert_eq!(Bitrate::video_stream_bps(1.0).source, BitrateSource::Declared);
        let measured = Bitrate::video_stream(1000, Some(1.0)).expect("rate");
        assert_eq!(measured.source, BitrateSource::Measured);
        assert_eq!(Bitrate::overall(1000, Some(1.0)).expect("rate").source, BitrateSource::Measured);
        assert_eq!(measured.declared().source, BitrateSource::Declared);
        assert_eq!(measured.declared().scope, BitrateScope::VideoStream, "declared() keeps scope");
    }

    #[test]
    fn schema_version_matches_the_documented_one() {
        assert_eq!(SCHEMA_VERSION, "3.0");
        let v = serde_json::to_value(maximal_report()).unwrap();
        assert_eq!(v["hdrprobe_schema_version"], "3.0");
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
