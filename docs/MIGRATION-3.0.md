# Migrating to hdrprobe JSON schema 3.0

A quick migration guide for downstream consumers of hdrprobe's JSON output. Schema 3.0 ships
in hdrprobe 0.9.0.

## What changed and why

Schema 2.x reported Dolby Vision colour and compatibility facts only where a container happened
to declare them. Dolby's own specification *defines* many of those facts outright: a profile
fixes its cross-compatibility id, and an id fixes the base layer's colour. hdrprobe now resolves
them, and says per field where each value came from so a derived value is never mistaken for a
signalled one.

Five changes can break a working 2.x consumer. Four are string or field changes you can grep
for; the fifth is a presence change, which is the one worth reading carefully.

## The mechanical migration

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
   Profile 10 reports `"10.1"` rather than `"10"`. If you parse the profile string, a pattern
   expecting a bare major on raw streams will stop matching; `"<major>.<minor>"` is now the
   normal shape, with only an unresolvable Profile 10 or 20 printing a bare major.

   `compatibility` also gains `"Ultra HD Blu-ray-compatible"` for id 6. Every Profile 7 title
   previously reported id 6 with no name at all, so a lookup keyed on that field will start
   producing a value where it used to find nothing.

4. **`hdr.format` drops the `(fallback)` suffix from Dolby Vision base tags.** The word was not
   Dolby's terminology and no other layered format's base carried it:

   - `"Dolby Vision / HDR10 (fallback)"` becomes `"Dolby Vision / HDR10"`
   - `"Dolby Vision / HLG (fallback)"` becomes `"Dolby Vision / HLG"`
   - `"Dolby Vision / SDR (fallback)"` becomes `"Dolby Vision / SDR"`
   - `"Dolby Vision / HDR10+ / HDR10 (fallback)"` becomes `"Dolby Vision / HDR10+ / HDR10"`

   If you matched the whole string, match the unsuffixed form. If you were testing for an HDR10
   base, that now needs one comparison instead of two, and splitting on `" / "` and testing for
   the `HDR10` component is more robust than either.

5. **`color.matrix`'s IPT value is respelled** `"IPT-PQ-C2"` (was `"IPT-PQ-c2"`), per SMPTE
   ST 2128:2023 and both revisions of Dolby's specification. A case-sensitive comparison needs
   updating; a case-insensitive one is unaffected.

6. **`color.primaries`, `color.transfer` and `color.matrix` now appear on Dolby Vision inputs
   that signalled nothing.** This is the presence change, and it is the one a consumer can miss
   because nothing errors: the fields simply start existing.

   A Profile 5 track that reported `{"range": "full"}` now reports all four fields. A Profile 4
   track that reported `{}` now reports Rec.709. The values come from the colour the profile and
   compatibility id define, and are tagged `spec` in the new `color_source` object.

   **If your code read an absent field as "the stream did not signal this", read
   `color_source` instead.** `spec` is the derived case; `container`, `stream` and `sei` are the
   signalled ones. For example, "did this file actually carry a transfer characteristic" is
   `.video_tracks[].color_source.transfer != "spec"`, not `has("transfer") | not`.

## New, and safe to ignore

- **`video_tracks[].color_source`** is always present and mirrors `color` field for field: the
  two objects always carry the same key set. Values are `container` (an MP4 `colr`/`vpcC` or
  MKV `Colour`), `stream` (an SPS/sequence-header VUI or a VP9/ProRes frame header), `sei` (the
  HLG/PQ alternative-transfer-characteristics message) and `spec` (defined by the DV profile and
  id). Per field rather than per object because they genuinely differ within one track: a
  Profile 5 stream signals its range and derives the other three.
- **`dolby_vision.pq_reshaping`** (true only) marks a base layer whose real transfer
  characteristic is Dolby's "PQ with reshaping" rather than the plain PQ its VUI names. Dolby
  states this for cross-compatibility id 0. It has no CICP code point, which is why it sits here
  and not in `color.transfer`.
- **`dolby_vision.deprecated_combination`** (true only) marks the profile/id pairings Dolby has
  withdrawn, `8.3` and `8.5`. Profile 8 itself is current, and the legacy *profiles* are not
  flagged.
- **The three CICP fields now name every code point ITU-T H.273 defines**, rather than a common
  subset. Values such as `"BT.601 (NTSC)"` (matrix 6, ordinary standard-definition video),
  `"Linear"` and `"sRGB (IEC 61966-2-1)"` are reported where they were previously omitted as
  unrecognized. If you match on the old, shorter value sets, widen the match or fall through on
  unknown names. The same widening applies to `sl_hdr.target_primaries`, which shares
  `color.primaries`' value space.

## Notes

- Inputs with no Dolby Vision metadata see only the new `color_source` object and the wider CICP
  value sets. Their `hdr.format`, `color` values and everything else are unchanged.
- Metadata sidecars (raw RPU `.bin`, DV CM XML, HDR10+ JSON) report no colour at all, before or
  after: they have no base layer to describe, so the spec fill never runs for them.
- A typical jq migration for the two most likely breakages:
  `.video_tracks[].hdr.format | test("HDR10")` is unaffected, but
  `.video_tracks[].hdr.format == "Dolby Vision / HDR10 (fallback)"` becomes
  `== "Dolby Vision / HDR10"`; and `.video_tracks[].dolby_vision.profile_compat_assumed`
  becomes `.video_tracks[].dolby_vision.compat_source == "assumed"`.

Full field-by-field reference: [SCHEMA.md](SCHEMA.md).
