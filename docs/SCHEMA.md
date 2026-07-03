# hdrprobe JSON output schema

This document is the field-by-field reference for hdrprobe's machine-readable output, the
contract external scripts can rely on. It describes **schema version 1.0** (as of hdrprobe
0.1.0) and is maintained against the report model in `src/model.rs`; every object and field
that can appear in the output is listed here, together with the conditions under which it
appears.

## Output modes

Two flags produce machine-readable output. Both serialize the same `Report` object; they differ
only in framing.

| Invocation | Shape |
|---|---|
| `--json` (or `--format json`) with one input file | A single pretty-printed `Report` object |
| `--json` with multiple input files (or a directory argument that expands to several) | A pretty-printed array of `Report` objects, one per successfully parsed file |
| `--format ndjson` | One compact `Report` object per line, one line per successfully parsed file |

The object-vs-array decision under `--json` follows the number of *collected* input paths, not
the number of command-line arguments: a directory argument containing one file still yields a
single object, and a directory containing several yields an array.

A file that cannot be parsed produces no `Report`. The error goes to stderr and the process exit
code becomes `2`; the remaining files are still reported. Consequences for consumers:

- Under `--json`, a run where every input failed prints `[]` (an empty array), including the
  single-input case.
- Under `--format ndjson`, a failed file simply contributes no line.

Exit codes: `0` all inputs parsed, `1` usage error (no output), `2` at least one input was
unreadable or corrupt.

## Consuming the output

**Streams.** JSON goes to stdout; diagnostics go to stderr. The two never mix, so stdout is
always parseable regardless of how many inputs failed. `-o <path>` writes the same bytes to a
file instead of stdout; there is no difference in content, so shell redirection and `-o` are
interchangeable.

**Choosing a mode.** `--json` suits one-shot inspection of a known input count. For scanning
libraries or piping through line-oriented tools, prefer `--format ndjson`: it always emits
exactly one `Report` object per line, so consumers need no object-vs-array handling and can
process results as they stream, without holding a whole scan in memory.

Shell, with `jq`:

```sh
# One file: a single object
hdrprobe --json movie.mkv | jq -r '.dolby_vision.profile'

# Library scan: one object per line, filter as a stream
hdrprobe --format ndjson -r ./library |
  jq -r 'select(.dolby_vision) | [.file, .dolby_vision.profile] | @tsv'
```

Python, as a subprocess:

```python
import json, subprocess

p = subprocess.run(
    ["hdrprobe", "--format", "ndjson", "-r", "./library"],
    capture_output=True, text=True,
)
reports = [json.loads(line) for line in p.stdout.splitlines()]
if p.returncode == 2:
    print("some inputs failed:", p.stderr)  # parsed files are still in stdout
```

**Handling optional fields.** Absent means omitted, never `null` (see Conventions). Use
lookups that tolerate missing keys: `report.get("dolby_vision")` in Python,
`.dolby_vision // empty` or `select(.dolby_vision)` in jq. A field's absence is meaningful:
it says the input did not carry that information, not that the value is zero or empty.

**Handling failures.** Exit code `2` means partial results, not total failure: stdout still
contains every file that parsed, and stderr names the ones that did not. A consumer that
treats `2` as fatal will discard good data. Under `--json` a run where every input failed
prints `[]`; under `--format ndjson` it prints nothing.

**Object vs array under `--json`.** The shape follows the number of collected files, so a
directory argument can yield either. A consumer that cannot predict its input count should
either use `--format ndjson` or normalize after parsing, e.g. in Python:
`reports = data if isinstance(data, list) else [data]`.

## Schema versioning

Every `Report` carries `hdrprobe_schema_version`, the version of **this document's schema**,
as a `"<major>.<minor>"` string. The current version is `"1.0"`.

The name is deliberately explicit about whose version it is, because the output contains two
other, unrelated version fields: `general.format_version` is the version an *input* sidecar
declares for itself (e.g. a Dolby Vision CM XML's own schema version), and
`dolby_vision.cm_version` is Dolby's content-mapping version. `hdrprobe_schema_version`
describes hdrprobe's output shape and nothing about the inspected file.

It is versioned independently of the hdrprobe release version, so an unchanged value across
releases means exactly what a consumer wants it to mean: the output contract did not change
and existing scripts need no update.

Bump policy:

- **Minor bump** (`1.0` to `1.1`): additive changes a conforming consumer survives. A new
  optional field, a new value in an enumerated string set (a new `container` label, a new
  `hdr.format` component, a corrected label spelling), a new object attached to an existing
  optional slot.
- **Major bump** (`1.x` to `2.0`): anything that can break a correct consumer. Renaming or
  removing a field, changing a field's type or unit, changing when a field is present, or
  changing the meaning of an existing value.

Consumer guidance: check the major version and fail loudly on an unexpected one; ignore the
minor version unless you depend on a feature it introduced. A consumer that follows the
conventions below (tolerate unknown fields, treat enumerated sets as open) is insulated
against every minor bump by construction.

The version is enforced in the codebase: a unit test in `src/model.rs` pins the full set of
serialized field paths and the version string, so any change to the output shape fails the
build until the schema version, that test, and this document are updated together.

## Conventions

- **Absent means omitted, never `null`.** Every optional field is skipped entirely when it has
  no value. No field in the schema is ever serialized as `null`.
- **Empty arrays are omitted.** `l5_active_areas` and `trim_targets` are absent rather than `[]`
  when nothing was found.
- **Default-false booleans may be omitted.** `profile_compat_assumed` appears only when `true`.
  All other booleans (`bl_present`, `el_present`, `rpu_present`, `sampled`, `zeroed`,
  `l11_reference_mode`) are serialized whenever their containing object is.
- **Numbers.** JSON has a single number type; the tables below note the underlying type.
  Integer-typed fields are always whole numbers. Float-typed fields (`fps`, `duration_secs`,
  `bits_per_sec`, `max_luminance`, `min_luminance`, `bl_max_nits`, `rpu_max_nits`, `elapsed_ms`)
  may carry fractional digits.
- **Key order is not significant.** Keys currently serialize in model order, but consumers
  should treat objects as unordered maps.
- **Enumerated strings are exact but the sets are open.** Where a table below lists a set of
  string values, those are the byte-exact strings emitted; there is no localization or case
  variation. New values may join a set in a minor schema version, so match the values you
  know and fall through gracefully on ones you do not.

## Object: `Report` (top level)

| Field | Type | Presence | Description |
|---|---|---|---|
| `hdrprobe_schema_version` | string | always | Version of hdrprobe's own output schema, `"<major>.<minor>"`; see Schema versioning above. Not related to the inspected file's metadata (contrast `general.format_version` and `dolby_vision.cm_version`) |
| `file` | string | always | The input path as given on the command line (or as found during a directory scan) |
| `size_bytes` | integer | always | File size in bytes |
| `general` | `General` | always | Container, codec, picture, and colour signalling |
| `hdr` | `Hdr` | video inputs only | Static HDR classification and mastering info; absent for metadata sidecars, which have no base layer |
| `dolby_vision` | `DolbyVision` | when DV metadata was found | Present when at least one RPU parsed, when the container carries a DV configuration (including under `--no-rpu`), or for a DV sidecar |
| `hdr10plus` | `Hdr10Plus` | when HDR10+ metadata was found | Present when ST.2094-40 metadata was parsed from the stream, or for an HDR10+ JSON sidecar. Like `dolby_vision`, the object's existence is the presence signal |
| `elapsed_ms` | float | always | Wall-clock parse time in milliseconds |

## Object: `General`

| Field | Type | Presence | Description |
|---|---|---|---|
| `container` | string | always | Container or sidecar kind; see the value table below |
| `codec` | string | always | `"HEVC"`, `"AVC"`, or `"AV1"`. The empty string `""` for metadata sidecars, which carry no video |
| `codec_profile` | string | optional | Codec profile label; see the format table below |
| `format_version` | string | optional | Sidecar schema version, e.g. `"4.0.2"` from a DV CM XML's root version. Only DV XML sidecars declare one today |
| `width` | integer | optional | Coded width in pixels; absent for sidecars and when the demux could not recover it |
| `height` | integer | optional | Coded height in pixels; same conditions as `width` |
| `fps` | float | optional | Frame rate. From container timing (MP4/MKV), the SPS VUI (TS, raw HEVC), the AV1 sequence header's timing info, averaged IVF timestamps, or a DV XML's `<EditRate>`. Absent when the input carries no rate signal; never guessed |
| `duration_secs` | float | optional | Duration in seconds. Absent when the input has no duration source (raw HEVC; raw AV1 OBU without a full scan; all sidecars) |
| `bitrate` | `Bitrate` | optional | Average bitrate; absent when no exact source and no duration exists |
| `bit_depth` | integer | optional | Luma bit depth (8, 10, or 12) |
| `chroma` | string | optional | Chroma subsampling: `"monochrome"`, `"4:2:0"`, `"4:2:2"`, `"4:4:4"` (a reserved signalling value renders `"?"`) |
| `stereo` | string | optional | Stereoscopic view structure from MP4 `vexu`/`stri` (MV-HEVC, DV Profile 20): `"Stereoscopic 3D (2 views)"`, `"Monoscopic (1 view)"`, or `"Multiview 3D (2+ views)"`. Absent for ordinary monoscopic video |
| `color` | `ColorInfo` | always | Colour signalling; may be `{}` when nothing was signalled |

### `container` values

Video inputs:

| Value | Source |
|---|---|
| `"MP4 (ISOBMFF)"` | MP4/MOV without a QuickTime `ftyp` brand |
| `"QuickTime (MOV)"` | ISOBMFF with the `qt  ` major brand |
| `"Matroska"` | MKV and WebM |
| `"MPEG-2 TS"` | Transport stream, 188-byte packets |
| `"MPEG-2 TS (M2TS/BDAV)"` | Transport stream, 192-byte packets |
| `"raw HEVC (Annex-B)"` | HEVC elementary stream |
| `"raw AV1 (IVF)"` | AV1 in an IVF wrapper |
| `"raw AV1 (OBU)"` | AV1 low-overhead OBU stream |

Metadata sidecars (no `hdr` section, empty `codec`):

| Value | Source |
|---|---|
| `"Dolby Vision RPU"` | Raw RPU stream (`.bin` / `.rpu`) |
| `"Dolby Vision XML"` | Dolby CM XML (DolbyLabsMDF) |
| `"HDR10+ JSON"` | hdr10plus_tool JSON |

### `codec_profile` formats

| Codec | Format | Examples |
|---|---|---|
| HEVC | `<profile>, <tier> tier @ L<level>` | `"Main 10, High tier @ L5.1"`, `"Main, Main tier @ L4"` |
| MV-HEVC | The HEVC label prefixed with `Multiview ` | `"Multiview Main 10, High tier @ L5"` |
| AVC | `<profile> @ L<level>` | `"High @ L4.2"`, `"Constrained High @ L4"`, `"Baseline @ L3.1"` |
| AV1 | `<profile> profile, <tier> tier @ L<level>` (level omitted when unset) | `"Main profile, Main tier @ L5.1"`, `"Main profile, Main tier"` |

## Object: `Bitrate`

| Field | Type | Presence | Description |
|---|---|---|---|
| `bits_per_sec` | float | always | Average rate in bits per second |
| `scope` | string | always | `"video_stream"` (exact encoded video byte count or a container-stated per-stream rate) or `"overall"` (file length divided by duration, which also counts audio and container overhead) |

## Object: `ColorInfo`

All four fields are optional; each is omitted when the input does not signal it or signals a
code hdrprobe does not name.

| Field | Type | Values |
|---|---|---|
| `primaries` | string | `"BT.709"`, `"BT.601 (PAL)"`, `"BT.601 (NTSC)"`, `"BT.2020"`, `"DCI-P3"`, `"Display P3"` |
| `transfer` | string | `"BT.709"`, `"BT.601"`, `"BT.2020 (10-bit)"`, `"BT.2020 (12-bit)"`, `"PQ (SMPTE ST 2084)"`, `"HLG (ARIB STD-B67)"` |
| `matrix` | string | `"RGB"`, `"BT.709"`, `"BT.2020 NCL"`, `"BT.2020 CL"`, `"IPT-PQ-c2"` |
| `range` | string | `"limited"`, `"full"` |

When an HLG/PQ preferred-transfer SEI (alternative transfer characteristics) is present, it
overrides the VUI value in `transfer`.

## Object: `Hdr`

Present for every video input; absent for sidecars.

| Field | Type | Presence | Description |
|---|---|---|---|
| `format` | string | always | Overall classification; see below |
| `mastering` | `MasteringDisplay` | optional | Base-layer mastering display, preferring the container box, then the ST.2086 SEI, then the DV L6 fallback |
| `content_light` | `ContentLight` | optional | MaxCLL/MaxFALL, preferring the container, then the SEI, then the DV L6 fallback |

### `format` values

The string is a ` + `-joined list built from, in order:

1. `Dolby Vision` when a DV section is present.
2. `HDR10+` when HDR10+ metadata is present.
3. A base-signal tag: `HDR10`, `HLG`, or `SDR` for non-DV content; `HDR10 (fallback)`,
   `HLG (fallback)`, or `SDR (fallback)` for the base layer under DV. Omitted entirely when the
   DV stream has no independently viewable base (Profile 5 and Profile 20, whose base is
   IPT-PQ-c2).

Examples: `"SDR"`, `"HDR10"`, `"HLG"`, `"HDR10+ + HDR10"`, `"Dolby Vision"`,
`"Dolby Vision + HDR10 (fallback)"`, `"Dolby Vision + HDR10+ + HDR10 (fallback)"`,
`"Dolby Vision + SDR (fallback)"`, `"Dolby Vision + HLG (fallback)"`.

## Object: `MasteringDisplay`

Used in two places with different meanings: `hdr.mastering` describes the **base layer's**
declared display (container MDCV box or ST.2086 SEI), while `dolby_vision.mastering_display`
describes the **DV grade's own** display (the RPU DM header's `source_min_pq`/`source_max_pq`,
or a DV XML's exact Level-0 values). On dual-layer titles the two can legitimately differ.

| Field | Type | Presence | Description |
|---|---|---|---|
| `max_luminance` | float | always | Peak luminance in cd/m² (nits) |
| `min_luminance` | float | always | Minimum luminance in cd/m² (nits); typically sub-1 values such as `0.0001` |
| `primaries` | string | optional | Named mastering gamut; omitted when the coordinates match no known gamut or the input carries none. Values: `"BT.2020"`, `"DCI-P3 D65"`, `"DCI-P3"`, `"BT.709"`, and (from a DV L9 predefined index only) `"SMPTE-C"`, `"BT.601"`, `"ACES"`, `"S-Gamut"`, `"S-Gamut-3.Cine"` |
| `primaries_level` | integer | optional | The Dolby metadata level the `primaries` name came from: `9` for an RPU L9 block, `0` for a DV XML's Level-0 global mastering display. Absent for container/SEI-derived primaries |

## Object: `ContentLight`

| Field | Type | Presence | Description |
|---|---|---|---|
| `max_cll` | integer | always | Maximum content light level, cd/m² |
| `max_fall` | integer | always | Maximum frame-average light level, cd/m² |

## Object: `DolbyVision`

| Field | Type | Presence | Description |
|---|---|---|---|
| `profile` | string | always | `<major>.<minor>` from the container config, e.g. `"8.1"`, `"5.0"`, `"10.4"`, `"20.0"`. Dual-layer profiles (4, 7) append the enhancement-layer kind: `"7.6 (FEL)"`, `"4.2 (MEL)"`. When no compatibility id is available the minor is a convention default for profiles 8 (`8.1`) and 4 (`4.2`); any other profile then prints its bare major, e.g. `"5"` |
| `profile_compat_assumed` | boolean | only when `true` | The minor digit above was supplied by convention rather than read from data. Set only for metadata-only sidecars (a raw RPU bin); a video input's base-layer signalling backs the inference, so it is never flagged there |
| `structure` | string | optional | Layer/track layout, present only for dual-layer content: `"Single track, dual layer"` or `"Dual track, dual layer"` |
| `level` | integer | optional | DV level from the container `dvcC`/`dvvC`; absent when there is no container config (raw streams, sidecars) |
| `bl_present` | boolean | always | Base layer present (container flag, else derived from the profile) |
| `el_present` | boolean | always | Enhancement layer present |
| `rpu_present` | boolean | always | RPU substream present |
| `el_type` | string | optional | `"FEL"` or `"MEL"`; absent for single-layer profiles and under `--no-rpu` (the kind is read from the RPU) |
| `bl_compatibility_id` | integer | optional | Raw `dv_bl_signal_compatibility_id` (0, 1, 2, 4, ...) from the container or a DV XML's declared profile; absent when neither carries it |
| `compatibility` | string | optional | Human name for the id above: `"no cross-compatibility"`, `"HDR10-compatible"`, `"SDR-compatible"`, `"HLG-compatible"`. Absent for ids outside that set |
| `cm_version` | string | optional | Content-mapping version from L254: `"CM v4.0"` or `"CM v2.9"`. Absent under `--no-rpu` |
| `l5_active_areas` | array of `ActiveArea` | omitted when empty | Distinct L5 active areas seen. A sampled set unless `--full` or a sidecar (both exhaustive) |
| `l5_assumed_canvas` | array `[width, height]` | optional | Present only for DV sidecars, which record no resolution: the canvas (3840x2160) the active-area dimensions were computed against |
| `mastering_display` | `MasteringDisplay` | optional | The DV grade's own mastering display (see the `MasteringDisplay` section). Its `primaries` is filled only from a recognized L9 (`primaries_level` 9) or a DV XML Level-0 (`primaries_level` 0); a CM v2.9 RPU carries no display gamut, so the field is luminance-only there |
| `fel_brightness_expansion` | `FelBrightnessExpansion` | optional | Metadata indication that the FEL likely carries brightness beyond the base layer; see below |
| `l6_fallback` | `L6Fallback` | optional | The RPU's L6 (ST.2086-shaped) fallback block |
| `l9_mastering` | string | optional | L9 mastering-display gamut name. The `MasteringDisplay.primaries` names plus `"custom"` (an L9 with unrecognized explicit chromaticities) and `"unknown"` (an unrecognized predefined index) |
| `l11_content` | string | optional | L11 content type: `"Cinema"`, `"Games"`, `"Sports"`, `"User-generated"`, `"Reserved"`, `"Unknown"` |
| `l11_reference_mode` | boolean | optional | L11 reference-mode flag; present only when L11 was seen |
| `trim_targets` | array of `TrimTarget` | omitted when empty | Distinct L2/L8 trim target displays, in nits, sorted ascending |
| `rpu_count` | integer | always | Number of RPUs successfully parsed (0 under `--no-rpu`) |
| `sampled` | boolean | always | `true` when the DV facts reflect sampling; `false` under `--full`, for sidecars (exhaustive by construction), and under `--no-rpu` (nothing was sampled) |
| `census` | `DvCensus` | optional | Exhaustive per-level census. Present under `--full` and for DV sidecars; absent for sampled video runs and `--no-rpu` |

## Object: `FelBrightnessExpansion`

The evidence pair behind the flag, both in cd/m² (nits). Set only for FEL video inputs whose
grade mastering max exceeds the base layer's declared mastering max by more than 10%. This is a
metadata verdict; hdrprobe never decodes pixels, so absence of the object is not proof of no
expansion.

| Field | Type | Presence | Description |
|---|---|---|---|
| `bl_max_nits` | float | always | The base layer's declared mastering max (container MDCV or ST.2086 SEI) |
| `rpu_max_nits` | float | always | The DV grade's mastering max (`source_max_pq`) |

## Object: `L6Fallback`

| Field | Type | Presence | Description |
|---|---|---|---|
| `max_cll` | integer | always | L6 MaxCLL, cd/m² |
| `max_fall` | integer | always | L6 MaxFALL, cd/m² |
| `max_mastering` | integer | always | L6 mastering max luminance, cd/m² |
| `min_mastering` | integer | always | L6 mastering min luminance in 0.0001 cd/m² units |
| `zeroed` | boolean | always | `true` when MaxCLL and MaxFALL are both zero (a common real-world defect) |

## Object: `TrimTarget`

| Field | Type | Presence | Description |
|---|---|---|---|
| `nits` | integer | always | Target display peak luminance, cd/m² |
| `levels` | array of integer | always | The metadata level(s) that produced this value: `[2]`, `[8]`, or `[2, 8]`. L10 never appears; it only defines the display an L8 trim points at |

## Object: `ActiveArea`

One distinct L5 active picture area. Offsets are the L5 crop values; `width`/`height` are the
canvas dimensions minus the opposing offsets. For sidecars the canvas is assumed (see
`l5_assumed_canvas`); for bitstreams it is the coded picture size.

| Field | Type | Presence | Description |
|---|---|---|---|
| `width` | integer | always | Active area width in pixels |
| `height` | integer | always | Active area height in pixels |
| `left` | integer | always | Left offset in pixels |
| `right` | integer | always | Right offset in pixels |
| `top` | integer | always | Top offset in pixels |
| `bottom` | integer | always | Bottom offset in pixels |

## Object: `DvCensus`

Exhaustive per-RPU statistics, only produced when every RPU in the title was scanned.

| Field | Type | Presence | Description |
|---|---|---|---|
| `scene_cuts` | integer | always | RPUs carrying a scene-refresh flag, i.e. the shot count |
| `dm_version_index` | integer | optional | `dm_version_index` from L254; absent when no L254 was present (CM v2.9) |
| `level_presence` | array of `LevelPresence` | always | Per-level presence counts, ordered by level |

## Object: `LevelPresence`

| Field | Type | Presence | Description |
|---|---|---|---|
| `level` | integer | always | DV metadata level number |
| `rpus_with` | integer | always | How many RPUs carried a block of that level |

## Object: `Hdr10Plus`

Present only when HDR10+ (ST.2094-40) metadata was found; its existence is the presence
signal, mirroring `dolby_vision`.

| Field | Type | Presence | Description |
|---|---|---|---|
| `application_version` | integer | always | Application version |
| `num_windows` | integer | always | Number of processing windows |
| `profile` | string | optional | Single-character ST.2094-40 profile: `"A"` (histogram only) or `"B"` (Bezier tone-mapping curve). Omitted when the profile could not be determined |
| `target_max_luminance` | integer | optional | Target display max luminance the grade was made for, cd/m². Omitted when zero or absent |

## How input kind and flags affect presence

**Video vs sidecar.** A video input always has `general` picture fields as available, an `hdr`
section, and codec identification. A metadata sidecar (raw RPU, DV XML, HDR10+ JSON) has no
picture data: `codec` is `""`, `hdr` is absent, and `width`/`height`/`duration_secs`/`bitrate`/
`bit_depth`/`chroma`/`stereo` are absent. A DV XML additionally provides `fps` (from
`<EditRate>`) and `format_version`; a raw RPU bin provides neither. An HDR10+ JSON sidecar has
no `dolby_vision` section; DV sidecars have no `hdr10plus` section.

**Default (sampled) run.** `dolby_vision.sampled` is `true`; `l5_active_areas` and
`trim_targets` are unions over the sampled RPUs and may be incomplete; `census` is absent.

**`--full`.** Every RPU is scanned: `sampled` is `false`, `census` appears, and TS inputs gain
an exact video-stream `bitrate`. DV sidecars behave like `--full` by construction (every RPU in
the file is read).

**`--no-rpu`.** The DV section is built from the container configuration alone: `profile`,
`structure`, `level`, the presence booleans, `bl_compatibility_id`, and `compatibility` can
appear; everything RPU-derived (`el_type`, `cm_version`, L5/L6/L9/L11 fields,
`mastering_display`, `trim_targets`, `census`) is absent, and `rpu_count` is `0`.

**`--samples <N>`.** Changes only how many seek points feed the sampled sets; it never changes
the schema.
