# hdrprobe JSON output schema

**Schema version: 2.3**

This document is the field-by-field reference for hdrprobe's machine-readable output, the
contract external scripts can rely on. It is maintained against the report model in
`src/model.rs`; every object and field that can appear in the output is listed here,
together with the conditions under which it appears.

## Contents

- [Output modes](#output-modes)
- [Consuming the output](#consuming-the-output)
- [Conventions](#conventions)
- [Schema versioning](#schema-versioning)
- [Object reference](#object-reference)
  - [`Report` (top level)](#report-top-level)
  - [Multiple video tracks](#multiple-video-tracks)
  - [Blu-ray ISO probes and `BdIso`](#blu-ray-iso-probes-and-bdiso)
  - [`VideoTrack`](#videotrack)
  - [`Bitrate`](#bitrate)
  - [`ColorInfo`](#colorinfo)
  - [`Hdr`](#hdr)
  - [`MasteringDisplay`](#masteringdisplay)
  - [`ContentLight`](#contentlight)
  - [`DolbyVision`](#dolbyvision)
  - [`FelBrightnessExpansion`](#felbrightnessexpansion)
  - [`MasteringPrimariesMismatch`](#masteringprimariesmismatch)
  - [`L6`](#l6)
  - [`TrimTarget`](#trimtarget)
  - [`ActiveArea`](#activearea)
  - [`MetadataCadence`](#metadatacadence)
  - [`DvCensus`](#dvcensus)
  - [`LevelPresence`](#levelpresence)
  - [`Hdr10Plus`](#hdr10plus)
- [How input kind and flags affect presence](#how-input-kind-and-flags-affect-presence)
- [Progress events (stderr)](#progress-events-stderr)
- [Version history](#version-history)

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

**Streams.** JSON reports go to stdout; diagnostics go to stderr. The two never mix, so stdout
is always parseable regardless of how many inputs failed. `-o <path>` writes the same bytes to
a file instead of stdout; there is no difference in content, so shell redirection and `-o` are
interchangeable. With `--full --progress json`, stderr additionally carries machine-readable
progress events (see "Progress events" below); stdout is unaffected.

**Choosing a mode.** `--json` suits one-shot inspection of a known input count. For scanning
libraries or piping through line-oriented tools, prefer `--format ndjson`: it always emits
exactly one `Report` object per line, so consumers need no object-vs-array handling and can
process results as they stream, without holding a whole scan in memory.

Shell, with `jq`:

```sh
# One file: a single object; per-track facts live in the video_tracks array
hdrprobe --json movie.mkv | jq -r '.video_tracks[].dolby_vision.profile'

# Library scan: one object per line, filter as a stream
hdrprobe --format ndjson -r ./library |
  jq -r '. as $r | .video_tracks[] | select(.dolby_vision)
         | [$r.file, .dolby_vision.profile] | @tsv'
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
treats `2` as fatal will discard good data.

**Object vs array under `--json`.** The shape follows the number of collected files, so a
directory argument can yield either. A consumer that cannot predict its input count should
either use `--format ndjson` or normalize after parsing, e.g. in Python:
`reports = data if isinstance(data, list) else [data]`.

## Conventions

- **Absent means omitted, never `null`.** Every optional field is skipped entirely when it has
  no value. No field in the schema is ever serialized as `null`.
- **Empty arrays are omitted.** `l5_active_areas` and `trim_targets` are absent rather than `[]`
  when nothing was found.
- **Default-false booleans may be omitted.** `profile_compat_assumed`, `level_derived`, and
  `unconverted_dual_layer_rpu` appear only when `true`. All other booleans (`bl_present`,
  `el_present`, `rpu_present`, `sampled`, `zeroed`, `l11_reference_mode`) are serialized
  whenever their containing object is.
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

## Schema versioning

Every `Report` carries `hdrprobe_schema_version`, the version of **this document's schema**,
as a `"<major>.<minor>"` string. The current version is the one stated at the top of this
document.

The name is deliberately explicit about whose version it is, because the output contains two
other, unrelated version fields: `format_version` is the version an *input* sidecar
declares for itself (e.g. a Dolby Vision CM XML's own schema version), and
`video_tracks[].dolby_vision.cm_version` is Dolby's content-mapping version.
`hdrprobe_schema_version` describes hdrprobe's output shape and nothing about the inspected
file.

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
conventions above (tolerate unknown fields, treat enumerated sets as open) is insulated
against every minor bump by construction.

The version is enforced in the codebase: a unit test in `src/model.rs` pins the full set of
serialized field paths and the version string, so any change to the output shape fails the
build until the schema version, that test, and this document are updated together.

The per-version change log is in [Version history](#version-history) at the end of this
document.

## Object reference

One `Report` per input file. The nesting, with array fields marked `[]` (every non-array
object below is a single optional or always-present sub-object of its parent):

```
Report
├─ bd_iso: BdIso                      (Blu-ray ISO probes only)
└─ video_tracks[]: VideoTrack         (always at least one entry)
   ├─ bitrate: Bitrate
   ├─ color: ColorInfo
   ├─ hdr: Hdr                        (video inputs only)
   │  ├─ mastering: MasteringDisplay
   │  └─ content_light: ContentLight
   ├─ dolby_vision: DolbyVision      (when DV metadata was found)
   │  ├─ mastering_display: MasteringDisplay
   │  ├─ fel_brightness_expansion: FelBrightnessExpansion
   │  ├─ mastering_primaries_mismatch: MasteringPrimariesMismatch
   │  ├─ l6: L6
   │  ├─ l5_active_areas[]: ActiveArea
   │  ├─ trim_targets[]: TrimTarget
   │  ├─ metadata_cadence: MetadataCadence
   │  └─ census: DvCensus
   │     └─ level_presence[]: LevelPresence
   └─ hdr10plus: Hdr10Plus            (when HDR10+ metadata was found)
```

`MasteringDisplay` is the one shape used in two places, with different meanings; see its
section.

### `Report` (top level)

| Field | Type | Presence | Description |
|---|---|---|---|
| `hdrprobe_schema_version` | string | always | Version of hdrprobe's own output schema, `"<major>.<minor>"`; see Schema versioning above. Not related to the inspected file's metadata (contrast `format_version` and `video_tracks[].dolby_vision.cm_version`) |
| `file` | string | always | The input path as given on the command line (or as found during a directory scan); `"-"` for a stdin probe |
| `size_bytes` | integer | always | File size in bytes. For a truncated stdin probe (`input_truncated` present) this is the bytes actually probed, not the source's size |
| `input_truncated` | boolean | stdin probes only, when true | The piped stream exceeded the head budget, so only a leading window was probed: `size_bytes` is the bytes probed, and facts derived from the payload span rather than a declared header (TS `duration_secs`, non-MP4 `bitrate`) are withheld. Absent for file probes and for stdin streams that ended within the budget; see the stdin paragraph under "How input kind and flags affect presence" |
| `container` | string | always | Container or sidecar kind; see the value table under `VideoTrack` below |
| `bd_iso` | `BdIso` | Blu-ray ISO probes only | Which BDMV playlist/clip was auto-selected as the main feature; see "Blu-ray ISO probes" below |
| `format_version` | string | optional | Sidecar schema version, e.g. `"4.0.2"` from a DV CM XML's root version. Only DV XML sidecars declare one today |
| `duration_secs` | float | optional | Duration in seconds, file-level (a multi-track file reports its longest track's presentation length; a multi-program TS shares one mux timeline). Absent when the input has no duration source (raw HEVC; raw AV1 OBU without a full scan; all sidecars; a truncated stdin TS probe, whose PCR span would describe the prefix, not the stream) |
| `video_tracks` | array of `VideoTrack` | always, at least one entry | One entry per video track; see "Multiple video tracks" below |
| `elapsed_ms` | float | always | Wall-clock parse time in milliseconds |

### Multiple video tracks

`video_tracks` always exists and always holds at least one entry, so consumers iterate it
unconditionally: a single-track file (the overwhelming majority) simply has one entry, and a
metadata sidecar has one entry too (with `codec: ""`). More than one entry means the container
carries genuinely **independent** video tracks: an MKV or MP4 with several video tracks (e.g. a
remux carrying a color and a black-and-white cut of the same title), or a multi-program
TS capture with one video stream per service. A Dolby Vision Profile-7 base+enhancement-layer
pair is **one logical track** and never produces two entries, whatever the mux shape (MP4
dual-`trak`, TS dual-PID, or an atypical dual-track MKV): the pair reports as a single entry
with `dolby_vision.structure` = `"Dual track, dual layer"`. Track order follows the container:
MKV by TrackNumber, MP4 by `trak` order, TS by program then PID.

### Blu-ray ISO probes and `BdIso`

A decrypted Blu-ray ISO (`.iso`, UDF image with a `BDMV` tree) is probed through its **main
feature**: hdrprobe parses every playlist under `BDMV/PLAYLIST`, selects the one with the
longest edit duration (duplicate and looped decoy segments are collapsed first; ties break on
total referenced stream bytes), and runs the ordinary TS/M2TS pipeline over the selected
playlist's largest clip. Everything in the report (`duration_secs`, per-track bitrate, codec,
HDR and Dolby Vision facts) describes **that clip**, exactly as if the mounted
`BDMV/STREAM/<clip>.m2ts` had been probed directly; `size_bytes` stays the whole image's size.
`container` is `"Blu-ray ISO (BDMV)"` and `bd_iso` records the selection:

| Field | Type | Presence | Description |
|---|---|---|---|
| `playlist` | string | always | Selected playlist file name, e.g. `"00800.mpls"` |
| `playlist_duration_secs` | float | always | The playlist's own edit duration (sum of its distinct segments). Distinct from the top-level `duration_secs`, which is the probed clip's transport-clock duration |
| `clip` | string | always | Probed clip file name under `BDMV/STREAM`, e.g. `"00055.m2ts"` |
| `clip_index` | integer | always | 1-based position of the probed clip among the playlist's distinct clips in playback order |
| `clip_count` | integer | always | Number of distinct clips in the selected playlist. `1` for the common single-clip feature; more for seamless-branching titles, where only the largest clip is probed |

AACS-encrypted images are detected (the clip fails TS sync-lock and an `AACS` directory is
present) and rejected with an error; hdrprobe never decrypts. DVD-Video ISOs and non-BDMV
UDF images error distinctly. A fragmented (non-contiguous) main clip is not supported and
errors rather than guessing.

### `VideoTrack`

| Field | Type | Presence | Description |
|---|---|---|---|
| `track_number` | integer | optional | Container-native track identity: MKV TrackNumber, MP4 `tkhd` track_ID, TS the base layer's PID. Absent where no such id exists (raw elementary streams, sidecars) |
| `program` | integer | optional | TS `program_number`; present only for a multi-program mux |
| `default` | boolean | optional | MKV FlagDefault; absent for containers without such a flag |
| `codec` | string | always | `"HEVC"`, `"AVC"`, `"AV1"`, `"VP9"`, or `"ProRes"`. The empty string `""` for metadata sidecars, which carry no video |
| `codec_profile` | string | optional | Codec profile label; see the format table below |
| `width` | integer | optional | Coded width in pixels; absent for sidecars and when the demux could not recover it |
| `height` | integer | optional | Coded height in pixels; same conditions as `width` |
| `fps` | float | optional | Frame rate. From container timing (MP4/MKV), the SPS VUI (TS, raw HEVC), the AV1 sequence header's timing info, averaged IVF timestamps, or a DV XML's `<EditRate>`. Absent when the input carries no rate signal; never guessed |
| `bitrate` | `Bitrate` | optional | Average bitrate; absent when no exact source and no duration exists. The `"overall"` (file-length) fallback rate appears only when this is the file's sole video track (an overall rate attributed to one of several tracks would be a wrong number) |
| `bit_depth` | integer | optional | Luma bit depth (8, 10, or 12) |
| `chroma` | string | optional | Chroma subsampling: `"monochrome"`, `"4:2:0"`, `"4:2:2"`, `"4:4:4"` (a reserved signalling value renders `"?"`) |
| `stereo` | string | optional | Stereoscopic view structure from MP4 `vexu`/`stri` (MV-HEVC, DV Profile 20): `"Stereoscopic 3D (2 views)"`, `"Monoscopic (1 view)"`, or `"Multiview 3D (2+ views)"`. Absent for ordinary monoscopic video |
| `color` | `ColorInfo` | always | Colour signalling; may be `{}` when nothing was signalled |
| `hdr` | `Hdr` | video inputs only | Static HDR classification and mastering info; absent for metadata sidecars, which have no base layer |
| `dolby_vision` | `DolbyVision` | when DV metadata was found | Present when at least one RPU parsed, when the container carries a DV configuration (including under `--no-rpu`), or for a DV sidecar |
| `hdr10plus` | `Hdr10Plus` | when HDR10+ metadata was found | Present when ST.2094-40 metadata was parsed from the input (an HEVC SEI, an AV1 metadata OBU, or the Matroska BlockAdditions carriage used by VP9 in WebM), or for an HDR10+ JSON sidecar. Like `dolby_vision`, the object's existence is the presence signal |

#### `container` values

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
| `"raw VP9 (IVF)"` | VP9 in an IVF wrapper (`VP90` FourCC) |
| `"Blu-ray ISO (BDMV)"` | Decrypted Blu-ray UDF image; the report describes the auto-selected main-feature clip (see "Blu-ray ISO probes" above) |

Metadata sidecars (one `video_tracks` entry with empty `codec` and no `hdr` section):

| Value | Source |
|---|---|
| `"Dolby Vision RPU"` | Raw RPU stream (`.bin` / `.rpu`) |
| `"Dolby Vision XML"` | Dolby CM XML (DolbyLabsMDF) |
| `"HDR10+ JSON"` | hdr10plus_tool JSON |

#### `codec_profile` formats

| Codec | Format | Examples |
|---|---|---|
| HEVC | `<profile>, <tier> tier @ L<level>` | `"Main 10, High tier @ L5.1"`, `"Main, Main tier @ L4"` |
| MV-HEVC | The HEVC label prefixed with `Multiview ` | `"Multiview Main 10, High tier @ L5"` |
| AVC | `<profile> @ L<level>` | `"High @ L4.2"`, `"Constrained High @ L4"`, `"Baseline @ L3.1"` |
| AV1 | `<profile> profile, <tier> tier @ L<level>` (level omitted when unset) | `"Main profile, Main tier @ L5.1"`, `"Main profile, Main tier"` |
| VP9 | `Profile <n> @ L<level>` (level omitted when the mux states none; only WebM CodecPrivate and MP4 `vpcC` carry one) | `"Profile 2 @ L4.0"`, `"Profile 2"` |
| ProRes | The profile name from the MOV/MP4 sample-entry FourCC; omitted entirely for Matroska, which carries no profile signal | `"422 HQ"`, `"4444 XQ"` |

### `Bitrate`

| Field | Type | Presence | Description |
|---|---|---|---|
| `bits_per_sec` | float | always | Average rate in bits per second |
| `scope` | string | always | `"video_stream"` (exact encoded video byte count or a container-stated per-stream rate) or `"overall"` (file length divided by duration, which also counts audio and container overhead) |

### `ColorInfo`

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

### `Hdr`

Present for every video input; absent for sidecars.

| Field | Type | Presence | Description |
|---|---|---|---|
| `format` | string | always | Overall classification; see below |
| `mastering` | `MasteringDisplay` | optional | Base-layer mastering display, preferring the container box, then the ST.2086 SEI, then the DV L6 values. Like `content_light`, the L6 fallback applies only on an HDR10 base (an `HDR10` tag in `format`), where L6 by definition mirrors the base layer's own static metadata. On any other base (IPT-PQ-c2, HLG, SDR) the L6 values merely restate the DV grade's own display, which `dolby_vision.mastering_display` already reports, so the field is omitted. A container or SEI value, when actually signalled, is always reported |
| `content_light` | `ContentLight` | optional | MaxCLL/MaxFALL, preferring the container, then the SEI, then the DV L6 values. MaxCLL/MaxFALL is HDR10 (CTA-861.3) convention, so the L6 fallback applies only on an HDR10 base (an `HDR10` tag in `format`); no other base consumes it, and on an IPT-PQ-c2 or HLG base L6 is typically a zeroed placeholder, so the field is omitted rather than echo noise. A container or SEI value, when actually signalled, is always reported |

#### `format` values

The string is a ` / `-joined list built from, in order:

1. `Dolby Vision` when a DV section is present.
2. `HDR10+` when HDR10+ metadata is present.
3. A base-signal tag: `HDR10`, `HLG`, or `SDR` for non-DV content; `HDR10 (fallback)`,
   `HLG (fallback)`, or `SDR (fallback)` for the base layer under DV. Omitted entirely when the
   DV stream has no independently viewable base (Profile 5 and Profile 20, whose base is
   IPT-PQ-c2).

Examples: `"SDR"`, `"HDR10"`, `"HLG"`, `"HDR10+ / HDR10"`, `"Dolby Vision"`,
`"Dolby Vision / HDR10 (fallback)"`, `"Dolby Vision / HDR10+ / HDR10 (fallback)"`,
`"Dolby Vision / SDR (fallback)"`, `"Dolby Vision / HLG (fallback)"`.

### `MasteringDisplay`

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

### `ContentLight`

| Field | Type | Presence | Description |
|---|---|---|---|
| `max_cll` | integer | always | Maximum content light level, cd/m² |
| `max_fall` | integer | always | Maximum frame-average light level, cd/m² |
| `zeroed` | boolean | always | `true` when MaxCLL and MaxFALL are both zero (a common real-world defect), mirroring `L6.zeroed` |

### `DolbyVision`

| Field | Type | Presence | Description |
|---|---|---|---|
| `profile` | string | always | `<major>.<minor>` from the container config, e.g. `"8.1"`, `"5.0"`, `"10.4"`, `"20.0"`. Dual-layer profiles (4, 7) append the enhancement-layer kind: `"7.6 (FEL)"`, `"4.2 (MEL)"`. When no compatibility id is available the minor is a convention default for profiles 8 (`8.1`), 7 (`7.6`, the only combination Dolby defines; an untouched BDMV M2TS carries no DV descriptor, so this is the common Blu-ray-original case) and 4 (`4.2`); any other profile then prints its bare major, e.g. `"5"` |
| `profile_compat_assumed` | boolean | only when `true` | The minor digit above was supplied by convention rather than read from data. Set only for metadata-only sidecars (a raw RPU bin); a video input's base-layer signalling backs the inference, so it is never flagged there |
| `structure` | string | optional | Layer/track layout, present only for dual-layer content: `"Single track, dual layer"` or `"Dual track, dual layer"` |
| `level` | integer | optional | DV level, from the container `dvcC`/`dvvC`/TS descriptor when one declares it. When no config carries a level (an authentic disc M2TS, where UHD-BD signals DV via the playlist rather than the PMT, or a raw elementary stream) it is derived from the coded stream's resolution and frame rate against the Dolby level table (smallest level admitting `width x height x fps` and the width) and flagged via `level_derived`. The derivation is a pixel-rate floor: the level's bitrate/tier axis is not probed. Absent for metadata sidecars (no coded stream) and when the frame rate is unknown |
| `level_derived` | boolean | only when `true` | The `level` above was derived from stream properties rather than declared by a container config |
| `bl_present` | boolean | always | Base layer present **in the reported logical track** (container flag, else derived from the profile). A dual-track mux declares `bl_present` 0 on the enhancement-layer sub-stream's own config ("no BL in *this* stream"); once that EL is folded into its base layer's track group the merged report says `true`, since the group holds both layers by construction. A genuinely BL-less input (an EL-only cut with no base layer in the mux) still reports `false` |
| `el_present` | boolean | always | Enhancement layer present in the reported logical track (same dual-track fold rule as `bl_present`) |
| `rpu_present` | boolean | always | RPU substream present |
| `el_type` | string | optional | `"FEL"` or `"MEL"`; absent for single-layer profiles and under `--no-rpu` (the kind is read from the RPU) |
| `unconverted_dual_layer_rpu` | boolean | only when `true` | The RPU carries the dual-layer composer payload (the NLQ block whose fingerprint is `el_type`) but the carriage has no enhancement layer: an explicit container config with `el_present` 0 (and no folded dual-track EL stream, which is an enhancement layer regardless of the declaration), or AV1 (whose DV carriage is single-layer by construction). The signature of a custom transcode that injected a UHD-BD Profile 7 RPU without converting it; the stray payload is inert for playback but misleads tools that guess a profile from the RPU (mkvmerge derives an AV1 `dvvC` compat id that way, producing out-of-spec profile strings like `"10.6"`). A provenance observation, not an error claim. Never set for metadata sidecars (no carriage to compare) or config-less raw HEVC (its EL may legitimately ride in-band). Renders as the `Unconverted RPU` chip on the Profile line |
| `reconstructed_bit_depth` | integer | optional | The composer's reconstructed signal bit depth, read verbatim from the RPU header's `vdr_bit_depth` field: `12` for Profile 7 FEL, `14` for Profile 4 FEL (never assumed from the profile). Present only when `el_type` is `"FEL"`, the one case where a real residual reconstructs beyond the 10-bit base layer; MEL and single-layer RPUs signal a value too, but it describes composer arithmetic precision rather than content depth, so it is withheld. Absent under `--no-rpu` |
| `bl_compatibility_id` | integer | optional | Raw `dv_bl_signal_compatibility_id` (0, 1, 2, 4, ...) from the container or a DV XML's declared profile; absent when neither carries it |
| `compatibility` | string | optional | Human name for the id above: `"no cross-compatibility"`, `"HDR10-compatible"`, `"SDR-compatible"`, `"HLG-compatible"`. Absent for ids outside that set |
| `cm_version` | string | optional | Content-mapping version from L254: `"CM v4.0"` or `"CM v2.9"`. Absent under `--no-rpu` |
| `l5_active_areas` | array of `ActiveArea` | omitted when empty | Distinct L5 active areas seen. A sampled set unless `--full` or a sidecar (both exhaustive) |
| `l5_assumed_canvas` | array `[width, height]` | optional | Present only for DV sidecars, which record no resolution: the canvas (3840x2160) the active-area dimensions were computed against |
| `mastering_display` | `MasteringDisplay` | optional | The DV grade's own mastering display (see the `MasteringDisplay` section). Its `primaries` is filled only from a recognized L9 (`primaries_level` 9) or a DV XML Level-0 (`primaries_level` 0); a CM v2.9 RPU carries no display gamut, so the field is luminance-only there |
| `fel_brightness_expansion` | `FelBrightnessExpansion` | optional | Metadata indication that the FEL likely carries brightness beyond the base layer; see below |
| `mastering_primaries_mismatch` | `MasteringPrimariesMismatch` | optional | The DV grade's L9 mastering gamut disagrees with the base layer's own signalled mastering primaries; see below |
| `l6` | `L6` | optional | The RPU's L6 block: MaxCLL/MaxFALL and the mastering luminances (the DV carriage of HDR10-style static metadata) |
| `l9_mastering` | string | optional | L9 mastering-display gamut name. The `MasteringDisplay.primaries` names plus `"custom"` (an L9 with unrecognized explicit chromaticities) and `"unknown"` (an unrecognized predefined index) |
| `l11_content` | string | optional | L11 content type, named per Dolby's L11 (Dolby Vision IQ) definitions: `"Default"`, `"Movies"`, `"Game"`, `"Sport"`, `"User Generated Content"`, or `"Unknown"` for values outside the published 0-4 range |
| `l11_white_point` | string | optional | L11 intended white point: `"D65"` (0, the default), `"D93"` (8), or `"code N"` for the other codes, which Dolby accepts but does not publicly name. Present only when L11 was seen |
| `l11_reference_mode` | boolean | optional | L11 reference-mode flag; present only when L11 was seen |
| `trim_targets` | array of `TrimTarget` | omitted when empty | Distinct trim target displays, in nits, sorted ascending: the L2/L8 trims read plus any L10-defined target displays (custom L8 targets, folded into the L8 set) |
| `rpu_count` | integer | always | Number of RPUs successfully parsed (0 under `--no-rpu`) |
| `sampled` | boolean | always | `true` when the DV facts reflect sampling; `false` under `--full`, for sidecars (exhaustive by construction), and under `--no-rpu` (nothing was sampled) |
| `metadata_cadence` | `MetadataCadence` | optional | Whether the dynamic metadata is authored shot-by-shot or frame-by-frame; see below. Present under `--full` and for DV sidecars; absent for sampled video runs (no adjacent frames to compare) and `--no-rpu` |
| `census` | `DvCensus` | optional | Exhaustive per-level census. Present under `--full` and for DV sidecars; absent for sampled video runs and `--no-rpu` |

### `FelBrightnessExpansion`

The evidence pair behind the flag, both in cd/m² (nits). Set only for FEL video inputs whose
grade mastering max exceeds the base layer's declared mastering max by more than 10%. This is a
metadata verdict; hdrprobe never decodes pixels, so absence of the object is not proof of no
expansion.

| Field | Type | Presence | Description |
|---|---|---|---|
| `bl_max_nits` | float | always | The base layer's declared mastering max (container MDCV or ST.2086 SEI) |
| `rpu_max_nits` | float | always | The DV grade's mastering max (`source_max_pq`) |

### `MasteringPrimariesMismatch`

The evidence pair behind the mastering-primaries-mismatch flag. Set only for video inputs where
both sides resolved to a recognized gamut name and the names differ: the DV grade's gamut from a
recognized L9 block (`dolby_vision.mastering_display.primaries` with `primaries_level` 9), and
the base layer's own declared mastering primaries from a *signalled* container MDCV box or
ST.2086 SEI (never a fallback value). Both names come from the same gamut matcher, so the
comparison is exact. The classic trigger is re-encode drift, such as a BT.2020-claiming MDCV
written by an encoder over a DCI-P3 D65 grade. Unrecognized coordinates on either side suppress
the comparison entirely, and a metadata sidecar (no base layer) never carries the object.

| Field | Type | Presence | Description |
|---|---|---|---|
| `bl_primaries` | string | always | The base layer's declared mastering gamut name (container MDCV or ST.2086 SEI) |
| `rpu_primaries` | string | always | The DV grade's mastering gamut name (L9) |

### `L6`

Reported in the bitstream's native integer units, unchanged from how the RPU stores them.
This is a deliberate contrast with `MasteringDisplay`, whose luminances are converted to
cd/m² floats: keeping the raw values here is lossless and matches what other RPU tools
(e.g. `dovi_tool info`) print. Only `min_mastering` is fixed-point; it is the one field in
the schema that needs a unit conversion to read in nits.

| Field | Type | Presence | Description |
|---|---|---|---|
| `max_cll` | integer | always | L6 MaxCLL, cd/m² |
| `max_fall` | integer | always | L6 MaxFALL, cd/m² |
| `max_mastering` | integer | always | L6 mastering max luminance, cd/m² |
| `min_mastering` | integer | always | L6 mastering min luminance in fixed-point units of 0.0001 cd/m²; divide by 10000 for nits (`50` means 0.005 cd/m², `1` means 0.0001 cd/m²) |
| `zeroed` | boolean | always | `true` when MaxCLL and MaxFALL are both zero (a common real-world defect) |

### `TrimTarget`

| Field | Type | Presence | Description |
|---|---|---|---|
| `nits` | integer | always | Target display peak luminance, cd/m² |
| `levels` | array of integer | always | The trim level(s) this value belongs to: `[2]`, `[8]`, or `[2, 8]`. `8` covers both read L8 trims and target displays defined by the title's global L10 metadata (a display index is a CM v4.0/L8 mechanism by construction, so an L10-defined display is a custom L8 target even when its per-shot trims sit outside the sample). L10 itself never appears |

### `ActiveArea`

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

### `MetadataCadence`

The authoring cadence of the dynamic metadata, decided by comparing consecutive frames' DM
payloads (extension blocks plus the grade's mastering range; the scene-refresh flag and the
composer/NLQ payload are excluded, so a shot's first frame compares equal to the rest of its
shot). Produced only when every frame's RPU was read in stream order: a `--full` video scan or
a DV sidecar (the RPU bin's frame stream, or the DV XML's declared shot/edit structure). The
verdict is `"per-frame"` when at least a quarter of the pairs changed, not an exact-zero test:
a shot-based title still changes at its scene cuts (one change per shot transition, so a few
percent of pairs at most), while per-frame analysis produces equal neighbours over static
stretches and duplicated frames yet stays far above the quarter line. The counts are included
so a consumer can see how decisive the verdict was.

| Field | Type | Presence | Description |
|---|---|---|---|
| `cadence` | string | always | `"per-shot"` (frames within a shot share one DM payload, the standard CM authoring workflow) or `"per-frame"` (each frame carries its own values, e.g. converted or per-frame-analyzed metadata) |
| `frame_pairs` | integer | always | Consecutive-frame DM comparisons made (frames minus 1 on an exhaustive read) |
| `changed_pairs` | integer | always | How many of those comparisons differed |

### `DvCensus`

Exhaustive per-RPU statistics, only produced when every RPU in the title was scanned.

| Field | Type | Presence | Description |
|---|---|---|---|
| `scene_cuts` | integer | always | RPUs carrying a scene-refresh flag, i.e. the shot count |
| `dm_version_index` | integer | optional | `dm_version_index` from L254; absent when no L254 was present (CM v2.9) |
| `level_presence` | array of `LevelPresence` | always | Per-level presence counts, ordered by level |

### `LevelPresence`

| Field | Type | Presence | Description |
|---|---|---|---|
| `level` | integer | always | DV metadata level number |
| `rpus_with` | integer | always | How many RPUs carried a block of that level |

### `Hdr10Plus`

Present only when HDR10+ (ST.2094-40) metadata was found; its existence is the presence
signal, mirroring `dolby_vision`.

| Field | Type | Presence | Description |
|---|---|---|---|
| `application_version` | integer | always | Application version |
| `num_windows` | integer | always | Number of processing windows |
| `profile` | string | optional | Single-character ST.2094-40 profile: `"A"` (histogram only) or `"B"` (Bezier tone-mapping curve). Omitted when the profile could not be determined |
| `target_max_luminance` | integer | optional | Target display max luminance the grade was made for, cd/m². Omitted when zero or absent |

## How input kind and flags affect presence

The per-track presence notes below all describe fields of a `video_tracks` entry.

**Video vs sidecar.** A video input's track entries carry picture fields as available, an
`hdr` section, and codec identification. A metadata sidecar (raw RPU, DV XML, HDR10+ JSON) has
no picture data: its single track entry has `codec` `""`, no `hdr`, and no `width`/`height`/
`bitrate`/`bit_depth`/`chroma`/`stereo` (and the report-level `duration_secs` is absent). A DV
XML additionally provides `fps` (from `<EditRate>`) and the report-level `format_version`; a
raw RPU bin provides neither. An HDR10+ JSON sidecar has no `dolby_vision` section; DV
sidecars have no `hdr10plus` section.

**Default (sampled) run.** `dolby_vision.sampled` is `true`; `l5_active_areas` and
`trim_targets` are unions over the sampled RPUs and may be incomplete (though the L10-defined
custom targets folded into the L8 set are title-global: the definition rides every RPU, so
those entries are complete from any sample); `census` and `metadata_cadence` are absent.

**`--full`.** Every RPU is scanned: `sampled` is `false`, `census` and `metadata_cadence`
appear, and TS inputs gain an exact video-stream `bitrate`. DV sidecars behave like `--full`
by construction (every RPU in the file is read).

**`--no-rpu`.** The DV section is built from the container configuration alone: `profile`,
`structure`, `level`, the presence booleans, `bl_compatibility_id`, and `compatibility` can
appear; everything RPU-derived (`el_type`, `reconstructed_bit_depth`, `cm_version`,
L5/L6/L9/L11 fields, `mastering_display`, `trim_targets`, `metadata_cadence`, `census`) is
absent, and `rpu_count` is `0`.

**`--samples <N>`.** Changes only how many seek points feed the sampled sets; it never changes
the schema.

**Stdin (`-`).** `hdrprobe -` probes a bounded head of a stream piped to stdin, for callers
whose media has no filesystem path (Kodi VFS URLs, media-server plugins, ranged HTTP fetches).
The format is sniffed from the first bytes and dispatched by magic, and hdrprobe reads only its
own head budget: 24 MiB when the stream sniffs as TS/M2TS (whose metadata rides the in-band SPS
about a GOP in), 16 MiB otherwise. When it stops reading, a pipe writer sees a broken pipe;
that is the normal success signal, not an error. A stream that ends within the budget is
complete and reports exactly like a file probe. A stream that exceeds it reports
`input_truncated: true`, `size_bytes` as the bytes probed, and `file` as `"-"`, and withholds
what a prefix cannot honestly state: a TS input's `duration_secs` (the PCR span would describe
the cut, not the stream) and every `bitrate` except MP4/MOV's `video_stream` rate (whose
sample-table sums are exact regardless of truncation). Declared header facts (MP4 `mvhd` and
MKV Segment-Info durations, resolution, color, HDR and Dolby Vision metadata from the sampled
head) report normally. The sampled union fields (`l5_active_areas`, `trim_targets`) draw only
on head frames: `dolby_vision.sampled` is `true` exactly as on a default file probe, but a
head-only window is likelier to miss mid-title variation (an aspect-ratio change, trims added
in later scenes) than a file probe's whole-file sample spread. Limits: no tail-dependent facts ever (TS runtime, MKV statistics-tag
bitrate, an MP4 whose `moov` sits at the end fails honestly), `--full` errors on `-` (a pipe
cannot be fully scanned), metadata sidecars are not detectable via stdin (their dispatch is
extension-based), and `-` may be given at most once per run. A practical integration guide,
including a Kodi/Python example, is in [INTEGRATION-STDIN.md](INTEGRATION-STDIN.md).

## Progress events (stderr)

A `--full` scan of a large file can run for minutes (it reads every access unit), so hdrprobe
can report progress while it works. `--progress json` emits one compact JSON object per
**stderr** line; the report stream on stdout is byte-identical with or without it. Progress
reporting exists only under `--full`: the default fast path finishes in milliseconds and never
emits events (nor does `--progress bar`, the human display, ever appear there).

Consumer rule: parse stderr line by line and skip any line that is not valid JSON; non-JSON
lines are human diagnostics (`error: ...`), which share the stream. Events carry
`hdrprobe_schema_version` and follow the same versioning and conventions as the `Report`
(unknown fields must be tolerated; absent means omitted).

Two event shapes, distinguished by `event`:

| Field | Type | Present | Meaning |
|---|---|---|---|
| `event` | string | always | `"progress"` or `"done"` |
| `hdrprobe_schema_version` | string | always | Same contract version as the reports |
| `file` | string | always | The input path, exactly as the report's `file` will render it |
| `file_index` | integer | always | 1-based position of this file in the run |
| `file_count` | integer | always | Total files in the run |
| `phase` | string | `progress` only | `"index"` (a demux-time whole-file walk) or `"scan"` (the per-frame scan) |
| `bytes_done` | integer | `progress` only | Bytes processed within the phase |
| `bytes_total` | integer | `progress` only | The phase's byte denominator |
| `percent` | number | `progress` only | `bytes_done / bytes_total`, one decimal |
| `elapsed_ms` | number | always | Milliseconds since this file's processing started |

Per file: each phase opens with a `percent: 0` line, updates are throttled (at most a few per
second), and the `scan` phase always closes with a `percent: 100` line; an `index` phase may
end short of 100 when the walk legitimately stops early, so treat the next phase's opening
line or the `done` event as the phase boundary, never a specific percentage. A `done` event
follows when the file's report is complete; a file that fails produces no `done` event (its
diagnostic line takes that place). Phases are sequential. Every container normally scans in a
single fused pass and emits only `scan`; an `index` phase appears only for the rare demux-time
rescue walks (a TS or raw HEVC stream whose head window held no SPS, a raw AV1 OBU stream
whose head held no sequence header). Percentages are monotonic within a phase. Events describe
pacing, not content: nothing in them appears in, or changes, the `Report`.

## Version history

- **2.3**: stdin input support (additive). `hdrprobe -` probes a bounded head of a stream piped
  to stdin, sniff-dispatched with a format-aware read budget. The new `Report.input_truncated`
  boolean appears (true only) when the stream exceeded the budget; `size_bytes` is then the
  bytes probed and prefix-derived facts (a TS input's `duration_secs`, every `bitrate` except
  MP4/MOV's exact `video_stream` rate) are withheld. `"-"` joins the `file` value space. File
  probes are unchanged. Ships in hdrprobe 0.7.0.
- **2.2**: Blu-ray ISO support (additive). `"Blu-ray ISO (BDMV)"` joins the `container` set,
  and the new optional `bd_iso` object on `Report` records which BDMV playlist and clip were
  auto-selected as the main feature; the rest of the report describes that clip through the
  ordinary TS/M2TS pipeline. Also adds `dolby_vision.level_derived` (additive): when no
  container config declares a DV level (authentic disc M2TS, raw elementary streams),
  `level` is now derived from the coded stream's resolution and frame rate against the
  Dolby level table and flagged with `level_derived: true`; a declared level always wins
  and stays unflagged. Ships in hdrprobe 0.6.0.
- **2.1**: added `dolby_vision.unconverted_dual_layer_rpu` (additive): true when the RPU
  carries the dual-layer composer payload but the carriage has no enhancement layer, the
  signature of a custom transcode that injected a Profile 7 RPU without converting it.
  Renders as the `Unconverted RPU` chip on the text report's Profile line. Also adds VP9
  support (additive value-set growth, no new fields): `"VP9"` joins the `codec` set with its
  own `codec_profile` format, and `"raw VP9 (IVF)"` joins the `container` set. Also adds
  ProRes support the same way: `"ProRes"` joins the `codec` set, with a `codec_profile`
  present only for MOV/MP4 carriage (the profile is signalled by the sample-entry FourCC,
  which Matroska does not carry). Ships in hdrprobe 0.5.0.
- **2.0**: breaking restructure for multi-video-track reporting. The per-track sections moved
  into the always-present `video_tracks` array (one entry per video track: one for ordinary
  files, one per independent track for a multi-track MKV/MP4 or multi-program TS, and one for
  sidecars, so consumers always iterate the array): the old `general` object's track-level
  fields (`codec`, `codec_profile`, `width`, `height`, `fps`, `bitrate`, `bit_depth`, `chroma`,
  `stereo`, `color`) and the old top-level `hdr`, `dolby_vision`, `hdr10plus` objects are now
  fields of a `video_tracks` entry, which also gains `track_number`, `program`, and `default`.
  The old `general.container`, `general.format_version`, and `general.duration_secs` moved to
  the top level; `general` itself is gone. No field changed type, unit, or meaning. Also adds
  `video_tracks[].dolby_vision.metadata_cadence`: the shot-based vs frame-by-frame authoring
  verdict with its evidence counts, present for `--full` video scans and DV sidecars. Ships in
  hdrprobe 0.4.0. A step-by-step consumer migration guide is in
  [MIGRATION-2.0.md](MIGRATION-2.0.md).
- **1.2**: added `dolby_vision.mastering_primaries_mismatch` (additive). `trim_targets` now
  also includes target displays defined by the title's global L10 metadata even when no read
  trim referenced them, folded into the L8 set (a semantic broadening of `levels: [8]`, no new
  field or value). Ships in hdrprobe 0.3.0.
- **1.1**: added `dolby_vision.reconstructed_bit_depth` (additive). Ships in hdrprobe 0.2.0.
- **1.0**: initial schema, as shipped in hdrprobe 0.1.0.
