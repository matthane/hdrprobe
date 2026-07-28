# Migrating to hdrprobe JSON schema 3.0

A quick migration guide for downstream consumers of hdrprobe's JSON output. Schema 3.0 is the
JSON contract that ships with hdrprobe 1.0.0; the two numbers are independent versionings
(`hdrprobe_schema_version` tracks the JSON shape, the tool version tracks the program). Every
breaking change of the cycle is batched into this one schema bump, so a consumer adapts once.

## What changed and why

Two themes. First, provenance: schema 2.x reported Dolby Vision colour and compatibility
facts only where a container happened to declare them, and said nothing about where a
bitrate, level or colour value came from. Dolby's own specification *defines* many of those
facts outright, and 3.0 resolves them. It also says, per field, where each value came from, so
a derived value is never mistaken for a signalled one. Second, shape: a 1.0.0 release should
stand on a foundation solid enough that later versions rarely need to break it, so the
contract's inconsistencies (sentinel values, one shape under three names, booleans that
needed a second field to read) are fixed here together rather than trickling out across
future major bumps.

## The renames and removals, in one table

Every renamed field keeps its place in the tree: the `video_tracks[]` array and all the nesting
around these fields are unchanged. Paths below are relative to a `video_tracks[]` entry, except
`elapsed_ms`, which was a top-level report field.

| 2.x | 3.0 | Notes |
|---|---|---|
| `elapsed_ms` (top level) | *(removed)* | Run telemetry, not file metadata; two runs over one file now serialize identically. Progress events keep their own `elapsed_ms` |
| `hdr.mastering` | `hdr.mastering_display` | One name for the shared shape |
| `sl_hdr.source_mastering` | `sl_hdr.source_mastering_display` | Likewise |
| `dolby_vision.profile_compat_assumed` (bool) | `dolby_vision.compat_source` (string) | See item 2 below |
| `dolby_vision.level_derived` (bool, true-only) | `dolby_vision.level_source` (`"declared"` / `"derived"`) | Present exactly when `level` is; declared levels, previously unmarked, are now tagged too |
| `dolby_vision.sampled`, `hdr_vivid.sampled` (bool) | `dolby_vision.coverage`, `hdr_vivid.coverage` (`"sampled"` / `"full"` / `"none"`) | See item 9 below |
| `dolby_vision.l11_content` / `l11_white_point` / `l11_reference_mode` | one `dolby_vision.l11` object (`content`, `white_point`, `reference_mode`) | The three ride one RPU block and always appear together |
| `dolby_vision.l5_assumed_canvas` (`[w, h]` array) | `dolby_vision.l5_assumed_canvas` (`{width, height}` object) | Same name and place; the schema's only bare tuple becomes an object |
| sidecar `codec` = `""` | *(absent)* | "Is this a sidecar" is now a missing key, not a sentinel; video inputs always carry `codec` |
| `chroma` = `"?"` (reserved code) | *(absent)* | Reserved signalling values name nothing and omit the field. No real file is affected |
| `color.matrix` = `"IPT-PQ-c2"` | `color.matrix` = `"IPT-PQ-C2"` | Respelled per SMPTE ST 2128:2023 and Dolby's specification |

## The semantic changes, read carefully

1. **Check `hdrprobe_schema_version`.** If the major is `3`, use the new behaviour; on `2.x`
   keep the old (or just require 3.0+).

2. **`dolby_vision.profile_compat_assumed` is gone.** It was a boolean that could not express
   the two middle cases, and is replaced by `dolby_vision.compat_source`, a four-valued string:

   | Value | Meaning | Fills `bl_compatibility_id`? |
   |---|---|---|
   | `declared` | read from a container `dvcC`/`dvvC`/TS descriptor, or a DV XML's `GenerateProfile` | yes |
   | `spec` | fixed by the profile's definition (4, 5, 7, 9, and the legacy 0-3, 6) | yes |
   | `inferred` | deduced from the base layer's signalled VUI, only when one candidate survives | yes |
   | `assumed` | Profile 8's convention default, no evidence | **no** |

   The direct replacement is `.dolby_vision.profile_compat_assumed == true` becomes
   `.dolby_vision.compat_source == "assumed"`. That rung still leaves `bl_compatibility_id` and
   `compatibility` absent, exactly as before, so nothing else about that case changed.

3. **`bl_compatibility_id`, `compatibility` and the `profile` minor digit now resolve for
   inputs that previously left them absent.** A raw Profile 5 elementary stream reported
   `"profile": "5"` with no id; it now reports `"5.0"` with `bl_compatibility_id: 0`. A raw
   Profile 10 reports `"10.1"`. If you parse the profile string, a pattern expecting a bare
   major on raw streams will stop matching; `"<major>.<minor>"` is now the normal shape, with
   only an unresolvable Profile 10 or 20 printing a bare major. `compatibility` also gains
   `"Ultra HD Blu-ray-compatible"` for id 6, which previously reported no name at all.

4. **`hdr.format` drops the `(fallback)` suffix from Dolby Vision base tags.**
   `"Dolby Vision / HDR10 (fallback)"` becomes `"Dolby Vision / HDR10"`, and likewise for
   `HLG` and `SDR`. Splitting on `" / "` and testing components is more robust than matching
   whole strings; better still, read the new `hdr.base` field, which carries the base tag
   directly.

5. **`color.primaries`, `color.transfer` and `color.matrix` now appear on Dolby Vision inputs
   that signalled nothing.** This is the presence change a consumer can miss because nothing
   errors: the fields simply start existing. A Profile 5 track that reported
   `{"range": "full"}` now reports all four fields, from the colour the profile and
   compatibility id define, tagged `spec` in the new `color_source` object.

   **If your code read an absent field as "the stream did not signal this", read
   `color_source` instead.** `spec` is the derived case; `container`, `stream` and `sei` are
   the signalled ones. "Did this file actually carry a transfer characteristic" is
   `.video_tracks[].color_source.transfer != "spec"`, not `has("transfer") | not`.
   The same fill now also covers MPEG-4 Part 2 and VC-1, whose specifications define what an
   absent colour signal means.

6. **The three CICP fields name every code point ITU-T H.273 defines**, rather than a common
   subset. Values such as `"BT.601 (NTSC)"`, `"Linear"` and `"sRGB (IEC 61966-2-1)"` are
   reported where they were previously omitted as unrecognized. Widen matches or fall through
   on unknown names; `sl_hdr.target_primaries` shares this value space.

7. **`input_truncated` now also appears on file probes.** Through 2.x it was stdin-only. It
   now also fires when a file's own container declares more bytes than the file holds (AVI
   RIFF segment sizes, ASF `File Properties.File Size`, FLV `onMetaData.filesize`,
   RealMedia's `DATA` chunk extent): a partial download, or a capture that never closed.
   Read it as "partial input" generally; the stdin test is `file == "-"`.

8. **MKV and ASF frame rates now read clean.** Those containers store the *period* of the
   authored rate quantized to a clock tick (nanoseconds / 100 ns), so 2.x reported the raw
   quotient: `23.976024167…` for a 23.976 remux. 3.0 decodes the quantization exactly: a
   stored period that is bit-for-bit the encoding of a standard rate reports that rate, so
   the same file now reads `fps: 23.976023976…` with `fps_rational: 24000/1001`,
   cross-container comparable with TS and MP4. This is a decode, not a snap: a period
   matching no standard rate's encoding keeps the raw tick ratio. A consumer that cached
   2.x float values will see MKV/ASF rates move in the sixth decimal place. Relatedly,
   FourCC codec fallbacks now trim their space padding (QuickTime `"dvc "` reports
   `"dvc"`), so `codec` agrees with `codec_id`.

9. **`sampled` became `coverage`, and reads better.** Whether the sampled-union fields
   (`l5_active_areas`, `trim_targets`, `target_max_luminances`) are complete previously took
   two fields: `sampled: false` meant either a full scan *or* `--no-rpu`'s
   nothing-was-read-at-all, disambiguated by `rpu_count`. Now one value states it:
   `"sampled"` (a spread of frames; unions may be incomplete), `"full"` (every frame read:
   `--full`, or a sidecar), `"none"` (no frame metadata read: `--no-rpu`, or a container
   declaration with no readable frame). `.sampled == true` becomes `.coverage == "sampled"`;
   a consumer that read `sampled == false` as "complete" should require `"full"`.

## New, and safe to ignore

All additive; a consumer that tolerates unknown fields needs no changes for these.

- **`video_tracks[].color_source`**: per-field provenance for `color` (`container` /
  `stream` / `sei` / `spec`), always present with the same key set as `color`.
- **`bitrate.source`**: `"measured"` (hdrprobe computed the rate from sums or actual bytes)
  or `"declared"` (a single header-stated rate or byte count: MKV statistics tags, ASF/FLV/
  RealMedia averages). Always present on every `bitrate`.
- **`hdr.base`**: the format string's base-signal tag (`"HDR10"` / `"HLG"` / `"SDR"`) as a
  field, absent when there is no independently viewable base (DV compatibility id 0).
- **`video_tracks[].codec_id`**: the container's own codec identifier beside the resolved
  `codec` name: the MP4 sample-entry FourCC (`"hvc1"` vs `"hev1"`, an encrypted track's
  recovered original), the Matroska CodecID, a TS `stream_type` in hex, and so on.
- **`video_tracks[].duration_secs`**: the track's own stated length, beside the file-level
  value.
- **`fps_rational`, `pixel_aspect_ratio_rational`, `display_aspect_ratio_rational`**:
  `{num, den}` reduced exact ratios beside the floats (`24000/1001` instead of a repeating
  decimal), present where the source states a ratio; measured or averaged rates carry none.
- **`dolby_vision.level_source`, `pq_reshaping`, `deprecated_combination`**: level
  provenance and two Dolby-published stream observations.
- **The `--errors` flag**: opt-in: a failed file contributes an error object
  (`{hdrprobe_schema_version, file, error}`) to the machine output beside the reports, so a
  scanner learns which files failed without parsing stderr. Discriminate on the `error` key,
  which a `Report` never carries.
- **New formats.** MPEG-1/2, MPEG-4 Visual, VC-1, MS-MPEG-4, MJPEG, Theora, VP8,
  Sorenson H.263, On2 VP6, DV and RealVideo join the `codec` set, and program streams, AVI,
  ASF, FLV, Ogg, raw DV, RealMedia and DVD-Video ISOs join the `container` set; files that
  previously produced no report at all now produce full ones. See SCHEMA.md's version history
  for the per-format details.

## Typical jq migrations

```sh
# was: .video_tracks[].dolby_vision.profile_compat_assumed
.video_tracks[].dolby_vision.compat_source == "assumed"

# was: .video_tracks[].dolby_vision.sampled
.video_tracks[].dolby_vision.coverage == "sampled"

# was: .video_tracks[].hdr.mastering.max_luminance
.video_tracks[].hdr.mastering_display.max_luminance

# was: testing .codec == "" for sidecars
.video_tracks[0] | has("codec") | not

# was: parsing the format string for the base signal
.video_tracks[].hdr.base
```

## Notes

- Inputs with no Dolby Vision metadata see the renames/removals, the new `color_source`
  object, and the wider CICP value sets; their `hdr.format` and `color` values are otherwise
  unchanged.
- Metadata sidecars (raw RPU `.bin`, DV CM XML, HDR10+ JSON) still report no colour values.
  `color` and `color_source` are present-but-empty `{}` objects, as everywhere: they have no
  base layer to describe, so the spec fill never runs for them.
- The text report and exit codes are unchanged across the whole bump; every change here is
  JSON-only.

Full field-by-field reference: [SCHEMA.md](SCHEMA.md).
