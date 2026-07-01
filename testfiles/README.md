# hdrprobe — test corpus

Test assets for hdrprobe. **Drop your clips into `integration/`** following the matrix below.

## Layout

- **`integration/`** — short real clips, one per profile / container / format case.
  **Local only — gitignored.** Real movie content is large and copyrighted; never commit it.
- **`fixtures/`** — tiny raw RPU `.bin` files (metadata only, no picture content).
  License-clean and **committed**. We extract these from your clips later with
  `dovi_tool extract-rpu` for the level-parsing unit tests.

Full-length files work (the tool only reads the bytes it needs), but ~15–30 s clips keep
this directory small. See **Making small clips** below — always stream-copy, never re-encode.

## Minimal set to start (Milestone 1)

These three get `hdrprobe` working end-to-end:

| File | What | Why |
|---|---|---|
| `dv81_hevc.mkv` *or* `dv5_hevc.mp4` | one single-track DV (Profile 8.1 or 5), HEVC | the happy path — RPU parse + render |
| `hdr10_hevc.mkv` | plain HDR10, **no** DV | HDR section + "no DV" negative case |
| `sdr.mp4` | plain SDR (BT.709) | pure negative — should say "no HDR/DV" |

## Full matrix

> **Corpus status (actual).** Profiles **5, 7-FEL, 7-MEL, 8.1, 8.4, 10** are all present across
> MKV / MP4 / M2TS / raw-HEVC / raw-AV1 (IVF + OBU). **8.4** is the official Dolby *Patterns of
> Nature* file (`Patterns_Of_Nature_HLG-P8.4_*.mp4`). **8.2 is a known gap** — not synthesizable
> with dovi_tool/ffmpeg/mkvmerge (no mode or editor sets compat = 2, it isn't stored in the RPU,
> and muxers only emit 1/4/0). Insight: the **BL compatibility id lives in the `dvcC`/`dvvC` box**
> (correlated with BL transfer — PQ→1, HLG→4), **not in the RPU**.

### DV profiles & containers

| Suggested name | Profile / codec | Container | Needed by | Notes |
|---|---|---|---|---|
| `dv5_hevc.mp4` | P5, HEVC | MP4 | M1 | DV-only, not HDR10-compatible (streaming rips) |
| `dv5_hevc.hevc` | P5/P8, HEVC | raw Annex-B | M1 | no container box → infer profile from RPU |
| `dv81_hevc.mkv` | P8.1, HEVC | MKV | M2 | most common: "DV + HDR10 fallback" |
| `dv10_av1.mp4` | P10, **AV1** | MP4 (`av01`) | M5 | T.35 OBU path |
| `dv10_av1.mkv` | P10, AV1 | MKV (`V_AV1`) | M5 | |
| `dv10_av1.ivf` | P10, AV1 | raw IVF | M5 | generated: `ffmpeg -c:v copy -f ivf` from the mp4 |
| `dv10_av1.obu` | P10, AV1 | raw low-overhead OBU | M5 | generated: `ffmpeg -c:v copy -f obu` from the mp4 |
| `dv7fel_dt_hevc.m2ts` | P7 FEL, HEVC | M2TS **dual-PID** | M6 ✅ | UHD-BD: BL PID 0x1011 (4K), EL+RPU PID 0x1015 (private 0x06, DV descriptor tag 0xB0). ffprobe misreads EL as `bin_data`; we read it via the PMT. Verified vs MediaInfo. |
| `dv7fel_dt_hevc.mp4` | P7 FEL, HEVC | MP4 **dual-track** | M7 ✅ | Two `trak`s: track 0 `hev1` 4K BL, track 1 `dvhe` 1080p EL+dvcC+RPU. We merge widest-dims BL + EL dvcC + both tracks' samples. No colr/mdcv boxes → colour recovered from in-band BL SPS. Different master from the `.m2ts`. |
| `dv84_hevc.ts` | P8.4 (HLG-compat), HEVC | MPEG-TS | M6 | broadcast-style (optional) |
| `dv7fel_hevc.mkv` | **P7 FEL** | MKV **single-track dual-layer** | M7 ✅ | mkvmerge folds BL+EL into one video track (EL residual in `BlockAdditions`); **RPU is in the main track** → FEL from the RPU since M2. No EL read needed. |
| `dv7mel_hevc.mkv` | **P7 MEL** | MKV **single-track dual-layer** | M7 ✅ | minimal EL; MEL distinguished from the RPU NLQ. |

### HDR (non-DV)

| Name | What | Notes |
|---|---|---|
| `hdr10plus_hevc.mkv` | HDR10+ (ST.2094-40), no DV | HDR10+ detector |
| `hlg_hevc.mp4` | HLG | transfer-characteristic detection |
| `dv81_hdr10plus_hevc.mkv` | **DV P8.1 *and* HDR10+ together** | both detectors must fire — real files exist |

### Edge cases (catch the subtle bugs)

| Name | What it stresses |
|---|---|
| `dv81_l5varies.mkv` | L5 active-area **changes** mid-title (variable aspect — IMAX/Nolan titles) → "distinct L5 set, sampled". *Cut a segment that spans the aspect change.* |
| `dv81_l6zero.mkv` | L6 MaxCLL/FALL = **0** (common real defect) → zero-flag |
| `dv_cm29.mkv` / `dv_cm40.mkv` | CM **v2.9** vs **v4.0** grade (L254) |
| `dv82_hevc.mkv` | P8.2 (SDR-compat) — **not synthesizable; documented gap** (see status note above) |
| `corrupt.mkv` | truncated file → error path / exit code 2 |
| `notvideo.mkv` | audio-only / wrong content → graceful handling |

(Profile 4 is legacy — skip it.)

## Naming convention

- DV: `dv<profile>_<codec>.<ext>` — e.g. `dv81_hevc.mkv`, `dv7fel_hevc.mkv`, `dv10_av1.ivf`.
- Non-DV: prefix `hdr10_`, `hdr10plus_`, `hlg_`, `sdr`.
- Edge suffixes: `_l5varies`, `_l6zero`, `_cm29`, `_cm40`.

## Making small clips that keep DV

Use a **stream-copy split**, never a re-encode:

```bash
# MKV — single-track AND P7 dual-track (preserves RPU + EL block additions). Best option.
mkvmerge -o dv81_hevc.mkv --split parts:00:00:00-00:00:20 source.mkv

# MP4 (P5/P8) — modern ffmpeg keeps the dvcC/dvvC box + RPU on copy
ffmpeg -i source.mp4 -t 20 -map 0:v:0 -c copy -movflags +faststart dv5_hevc.mp4

# M2TS / TS (P7, P8.4) — copies all PIDs
ffmpeg -i source.m2ts -t 20 -c copy dv7_hevc.m2ts

# Raw HEVC Annex-B (keeps type-62 RPU NALs)
ffmpeg -i source.mkv -map 0:v:0 -t 20 -c copy -bsf:v hevc_mp4toannexb -f hevc dv5_hevc.hevc

# Raw AV1 IVF
ffmpeg -i source_av1.mp4 -map 0:v:0 -t 20 -c copy -f ivf dv10_av1.ivf
```

**Verify after cutting** — some operations silently strip DV:

```bash
mediainfo dv81_hevc.mkv     # should still show the Dolby Vision profile
```

If a cut drops the metadata, start the cut on a keyframe or use the `mkvmerge` split.

> **Raw elementary streams (`.hevc`/`.ivf`/`.obu`) are special:** MediaInfo does **not**
> report Dolby Vision for them — it doesn't parse the RPU out of a raw stream — even when the
> RPU is fully present. Verify those by scanning for the DV payload instead: HEVC **NAL type
> 62**, or AV1 **ITU-T T.35 metadata OBUs** with Dolby provider code **`0x003B`** (`0x003C` =
> HDR10+) — or with `dovi_tool info`. Confirmed in the generated raws: ~720 RPU NALs per
> `.hevc`, 1450 Dolby T.35 OBUs per AV1 raw.

## Don't block on the rare ones

P8.2, P8.4, and especially **AV1 P10** are hard to source. If you can't find them, we
**synthesize**: `dovi_tool` converts P7→P8.x and generates RPUs from a JSON config, so we
can mux synthetic DV onto a free HDR10 clip. Grab what's easy first — the Milestone 1 trio
plus any P8.1 and a P7 Blu-ray rip is plenty to build real momentum.
