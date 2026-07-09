# Migrating to hdrprobe JSON schema 2.0

A quick migration guide for downstream consumers of hdrprobe's JSON output. Schema 2.0 ships
in hdrprobe 0.4.0.

## What changed and why

Schema 2.0 restructures the report so a file with several video tracks can report each one.
Everything that describes a video track moved into a new top-level `video_tracks` array.
Nothing changed type, unit, or meaning: fields just live at a new path.

## The mechanical migration

1. **Check `hdrprobe_schema_version`.** If the major is `2`, use the new paths; on `1.x` keep
   the old ones (or just require 2.0+).

2. **`general` is gone.** Its file-level fields moved to the top of the report:
   - `general.container` -> `container`
   - `general.duration_secs` -> `duration_secs`
   - `general.format_version` -> `format_version`

3. **Everything track-level moved into `video_tracks[]`.** That is the rest of the old
   `general` object (`codec`, `codec_profile`, `width`, `height`, `fps`, `bitrate`,
   `bit_depth`, `chroma`, `stereo`, `color`) plus the old top-level `hdr`, `dolby_vision`,
   and `hdr10plus` objects. So:
   - `.dolby_vision.profile` -> `.video_tracks[].dolby_vision.profile`
   - `.general.width` -> `.video_tracks[].width`
   - `.hdr10plus` -> `.video_tracks[].hdr10plus`

4. **`video_tracks` always exists and always has at least one entry**: one for ordinary
   files, one (with `codec: ""`) for metadata sidecars. You can iterate it unconditionally;
   if you only care about the common single-track case, `.video_tracks[0]` is the drop-in
   equivalent of the old top-level objects.

5. **New optional per-track fields** you can ignore or use: `track_number` (MKV TrackNumber /
   MP4 track_ID / TS PID), `program` (multi-program TS only), and `default` (MKV only).

6. **One behavioral nuance if you consume `bitrate`:** the `"overall"` (file length divided
   by duration) fallback rate only appears when the track is the file's sole video track,
   since attributing a whole-file rate to one of several tracks would be a wrong number.
   Exact per-stream rates are unaffected.

## Notes

- A Dolby Vision base+enhancement layer pair still reports as **one** track entry, so
  existing DV consumers will not suddenly see two entries for Profile 7 content. Multiple
  entries appear only for genuinely independent video tracks (multi-track MKV/MP4,
  multi-program TS).
- A typical jq migration: `.dolby_vision.cm_version` becomes
  `.video_tracks[].dolby_vision.cm_version` (or `.video_tracks[0]...` if you assume
  single-track).
- Also new in 2.0: `video_tracks[].dolby_vision.metadata_cadence`, the shot-based vs
  frame-by-frame authoring verdict, present for `--full` video scans and DV sidecars.

Full field-by-field reference: [SCHEMA.md](SCHEMA.md).
