# hdrprobe

Fast HDR and Dolby Vision metadata inspector.

hdrprobe is a single native binary that answers one question quickly: what HDR and Dolby
Vision metadata does this file actually carry? It does the work that normally requires
`mediainfo`, `ffprobe`, and `dovi_tool` together, without launching subprocesses in the hot
path, writing temp files, or extracting a full RPU stream to disk first. It memory-maps the
file and reads only the bytes it needs, so it stays fast regardless of file size.

```
movie.mkv  (225.14 MiB)

General
  Container        Matroska
  Duration         30s
  Bitrate          46.08 Mb/s   (video stream)
  Video            HEVC (Main 10, High tier @ L5.1) · 3840×2160 · 23.976 fps · 10-bit 4:2:0
  Color            BT.2020 · PQ (SMPTE ST 2084) · limited

HDR
  Format           Dolby Vision + HDR10+ + HDR10 (fallback)
  Mastering        max 1000  min 0.0001 cd/m²
  Content light    MaxCLL 737 · MaxFALL 130

Dolby Vision
  Structure        Single track, dual layer
  Profile          7 (MEL)   (BL+EL+RPU)
  Level            6   (max bit rate: 25 Mbps Main tier / 130 Mbps High tier)
  RPU / DM         present · CM v2.9 · MEL
  L5 active area   3840×1608  (2.39:1)  ·  L0 R0 T276 B276   [sampled]
  L6 fallback      MaxCLL 737 · MaxFALL 130
  Trim targets     100 nit   [L2/L8]

HDR10+
  Profile          B
  Application      v1
  Windows          1
  Target           400 nits
```

## What it reports

- General and video: container, codec and profile, resolution, frame rate, bit depth, chroma
  subsampling, and colour signalling (primaries, transfer, matrix, and range).
- HDR (static): classification across SDR, HDR10, HLG, HDR10+, and Dolby Vision, including
  their combinations, plus the mastering display (ST.2086) and MaxCLL / MaxFALL content light
  levels.
- Dolby Vision: profile (5, 7 FEL/MEL, 8.1, 8.4, 10, and related variants), level, presence of
  the base layer, enhancement layer, and RPU, base-layer compatibility, CM version (v2.9 or
  v4.0 via L254), and the title-stable dynamic levels: distinct L5 active areas, L6 fallback,
  L9 mastering, L11 content type, and the set of L2/L8 trim targets. Per-frame values such as
  L1 and per-shot trim values are deliberately omitted, since they are decode-time noise rather
  than title-level facts.
- HDR10+: presence, profile, application version, window count, and target display max
  luminance.

RPU parsing is native and in-process via [`libdovi`](https://github.com/quietvoid/dovi_tool)
(the `dolby_vision` crate); HDR10+ parsing uses the sibling `hdr10plus` crate.

## Supported inputs

| Container | Codecs | Notes |
|---|---|---|
| MP4 / MOV | HEVC, AV1 | includes Profile 7 dual-track (separate BL and EL `trak` boxes) |
| MKV / WebM | HEVC, AV1 | single-track, and Profile 7 single-track dual-layer |
| MPEG-TS / M2TS | HEVC | includes Profile 7 dual-PID (BL and EL on separate PIDs) |
| Raw HEVC | Annex-B | profile inferred from the RPU |
| Raw AV1 | IVF, low-overhead OBU | Dolby Vision Profile 10 |

hdrprobe recognises HEVC Dolby Vision profiles 5, 7 (FEL and MEL), and 8.x, and AV1 Profile
10.x.

### Metadata sidecar files

hdrprobe also reads standalone metadata files that carry no picture data. These bypass the
video pipeline entirely and are rendered through the same report, so text, JSON, and quiet
output all work unchanged.

| Input | Extension | Notes |
|---|---|---|
| Dolby Vision RPU | `.bin`, `.rpu` | Raw RPU stream (for example from `dovi_tool extract-rpu`), aggregated across every frame |
| Dolby Vision CM XML | `.xml` | Dolby CM metadata (DolbyLabsMDF), aggregated per shot |
| HDR10+ JSON | `.json` | hdr10plus_tool metadata; reports the file-level profile and the first scene from a bounded head read |

Each file is identified by content rather than extension alone, so an unrelated `.bin`,
`.xml`, or `.json` in a scanned directory is skipped. Because none of these formats records a
resolution, the L5 active-area dimensions for the Dolby Vision sidecars are computed against an
assumed UHD (3840x2160) master and labelled as assumed in the report.

A note on WebM: WebM is a subset of Matroska and is parsed by the same backend, but its codec
whitelist does not include HEVC. The common Dolby Vision profiles (5, 7, and 8) are all HEVC,
so they cannot appear in a spec-compliant WebM file. The only Dolby Vision profile that can is
Profile 10, which is carried in AV1. Because AV1 stores its RPU in-band as a metadata OBU,
hdrprobe detects it regardless of container, so a WebM carrying DV Profile 10 AV1 is reported
correctly. In practice this combination is rare.

## Install

Requires a Rust toolchain (1.85 or newer):

```sh
cargo build --release
# binary at target/release/hdrprobe
```

## Usage

```sh
hdrprobe movie.mkv                     # default report
hdrprobe *.mp4                         # several files
hdrprobe --sections dv movie.mp4       # just the Dolby Vision block
hdrprobe --json movie.mp4 | jq .dolby_vision.profile
hdrprobe --full movie.mkv              # exhaustive per-frame scan and census
hdrprobe --no-rpu disc.m2ts            # container-level DV config only
hdrprobe RPU.bin                       # inspect a raw Dolby Vision RPU sidecar
hdrprobe metadata.json                 # inspect an HDR10+ JSON sidecar
hdrprobe --format ndjson -r ./library > report.ndjson
```

A directory argument is scanned for video files. Add `-r` to descend into subdirectories.

### Options

| Flag | Effect |
|---|---|
| `-j, --json` | JSON instead of text (an array when several files are given) |
| `--format <fmt>` | `text` (default), `json`, or `ndjson` (one object per line) |
| `-f, --full` | Exhaustive per-frame scan: all distinct L5, a full trim-target census, and scene counts. Trades speed for completeness. |
| `--no-rpu` | Report the container DV configuration only, skipping RPU parsing. Effectively instant. |
| `-s, --samples <N>` | Number of seek points to sample (default 16). Higher values capture more distinct L5 areas. |
| `--sections <list>` | Comma-separated list drawn from `general,hdr,dv,hdr10plus` |
| `--color <when>` | `auto` (default, plain when piped), `always`, or `never` |
| `-q, --quiet` | One-line summary per file |
| `-r, --recursive` | Descend into directory arguments |
| `--threads <N>` | Number of parallel workers (default: logical core count) |
| `-o, --output <path>` | Write to a file instead of stdout |
| `-h, --help`, `-V, --version` | Standard |

Exit codes: `0` parsed successfully, `1` usage error, `2` unreadable or corrupt input.

## Performance

hdrprobe reads only the bytes it needs. The default path memory-maps the file, parses the
container index, and samples roughly 16 spread seek points in parallel using `rayon`. For
large raw elementary streams it NAL-splits only bounded byte windows rather than the whole
file, so cost stays flat regardless of file size. On remote volumes (SMB and NFS), it warms
the metadata region with a single pipelined read before parsing, so a scan does not fault
those bytes in over hundreds of round-trips. The `--full` flag lifts the sampling bounds for
an honest whole-stream census, trading speed for completeness.

Malformed input is handled gracefully. The third-party RPU and HDR10+ parsers can panic on
some inputs, so they are isolated with `catch_unwind`. One corrupt file never aborts a
directory scan.

## Trademarks

Dolby and Dolby Vision are trademarks of Dolby Laboratories, Inc. HDR10+ is a trademark of
HDR10+ Technologies, LLC. All other product names, logos, and brands are the property of their
respective owners.

hdrprobe is an independent tool. It is not affiliated with, endorsed by, or sponsored by Dolby
Laboratories or any other trademark holder named here. These names are used only to identify
the metadata formats the tool inspects.

## License

hdrprobe is licensed under the MIT License. See [LICENSE](LICENSE).

The release binary statically links a number of third-party Rust crates, all under permissive
licenses (MIT, Apache-2.0, ISC, Zlib, Unicode-3.0, and Unlicense). Their required copyright
notices and license texts are collected in [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md);
include that file when redistributing compiled binaries. RPU parsing uses
[`libdovi`](https://github.com/quietvoid/dovi_tool) (the `dolby_vision` crate) and HDR10+
parsing the sibling `hdr10plus` crate, both MIT-licensed.
