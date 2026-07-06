```
█ █ █▀▄ █▀█ █▀█ █▀█ █▀█ █▄▄ █▀▀
█▀█ █▄▀ █▀▄ █▀▀ █▀▄ █▄█ █▄█ ██▄  v0.2.1
```

Fast HDR, HDR10+, and Dolby Vision metadata inspector.

hdrprobe is a single native binary that answers one question quickly: what HDR and dynamic metadata does this file actually carry? It does the work that normally requires
`mediainfo`, `ffprobe`, `dovi_tool`, and `hdr10plus_tool` together, without launching subprocesses in the hot
path, writing temp files, or extracting a full RPU stream to disk first. It memory-maps the
file and reads only the bytes it needs, so it stays fast regardless of file size.

```
▮ movie.mkv 225.14 MiB

── GENERAL ─────────────────────────────────────────────────────
  Container         Matroska
  Duration          30s
  Bitrate           46.08 Mb/s · video stream
  Video             HEVC (Main 10, High tier @ L5.1) · 3840×2160 · 23.976 fps · 10-bit 4:2:0
  Color             BT.2020 · PQ (SMPTE ST 2084) · limited

── HDR ─────────────────────────────────────────────────────────
  Format            Dolby Vision / HDR10+ / HDR10 (fallback)
  Mastering         DCI-P3 D65 · max 1000  min 0.0001 cd/m²
  Content light     MaxCLL 737 · MaxFALL 130

── DOLBY VISION ────────────────────────────────────────────────
  Structure         Single track, dual layer
  Profile           7.6 (MEL)
  Content mapping   v4.0
  Mastering         DCI-P3 D65 L9 · max 1000  min 0.0001 cd/m²
  Trim targets*     100 nits L2/L8, 600 nits L2, 1000 nits L2
  L5 offsets*       L0 R0 T276 B276
  L5 active area    3840×1608  (2.39:1)
  L6 content light  MaxCLL 737 · MaxFALL 130
  L11 APO           Movies · white point D65 · reference mode

── HDR10+ ──────────────────────────────────────────────────────
  Profile           B
  Application       v1
  Windows           1
  Target            400 nits

* sampled from a spread of RPUs; --full reads every one
```

## What it reports

### General video

Container, codec and profile, resolution, frame rate, bit depth, chroma subsampling, colour
signalling (primaries, transfer, matrix, and range), and stereoscopic / multiview structure
(MV-HEVC, such as Dolby Vision Profile 20).

### HDR

The static mastering display characteristics (ST.2086 min/max luminance and the mastering
gamut, named when recognized: BT.2020, DCI-P3 D65, DCI-P3, or BT.709) and the MaxCLL / MaxFALL
content light levels.

### Dolby Vision

Both the fixed identity of the stream and its title-stable dynamic metadata.

**Identity.** The profile, in `profile.compatibility` form, where the second digit is the base
layer's cross-compatibility id:

| Profile | Codec | Reported format(s) |
|---|---|---|
| 4 | HEVC, dual-layer | `4.2` (FEL or MEL) |
| 5 | HEVC | `5.0` |
| 7 | HEVC, dual-layer | `7.6` (FEL or MEL) |
| 8 | HEVC | `8.1`, `8.4` |
| 9 | AVC | `9.2` |
| 10 | AV1 | `10.0`, `10.1`, `10.4` |
| 20 | MV-HEVC | `20.0`, `20.4` |

Alongside it, the track and layer structure (for dual-layer content), the content-mapping
version (`v2.9`, or `v4.0` via L254), the reconstructed signal bit depth for full-enhancement
(FEL) streams (12-bit on Profile 7, 14-bit on Profile 4), and the DV grade's own mastering
display, which can differ from the base layer's: on a Profile 7 title a 4000-nit grade can sit
over a 1000-nit HDR10 base.

**Dynamic levels.** The distinct values seen across the title: `L5` offsets and active areas,
`L6` content light, `L9` mastering gamut, `L11` APO (content type), and the set of `L2` / `L8`
trim targets.

**Deliberately omitted.** The per-frame and per-shot analysis levels (`L1` brightness, `L3` L1
offsets) and the per-shot trim values. These vary shot to shot rather than describing the title,
so they collapse to nothing meaningful once sampled or aggregated.

RPU parsing is native and in-process via [`libdovi`](https://github.com/quietvoid/dovi_tool)
(the `dolby_vision` crate); HDR10+ parsing uses the sibling `hdr10plus` crate.

### HDR10+

Presence, profile, application version, window count, and target display max luminance.

## Supported inputs

hdrprobe reads both video containers and standalone metadata sidecar files.

**Video containers.**

| Container | Codecs | Notes |
|---|---|---|
| MP4 / MOV | HEVC, AVC, AV1 | Single or dual track; an enhancement layer may ride its own track |
| MKV / WebM | HEVC, AVC, AV1 | Single track; an enhancement layer may be interleaved into it |
| MPEG-TS / M2TS | HEVC, AVC | Single or dual track; an enhancement layer may ride its own track |
| Raw HEVC | Annex-B | Elementary stream; profile inferred from the RPU |
| Raw AV1 | IVF, low-overhead OBU | Elementary stream; the RPU rides an in-band metadata OBU |

Containers are matched by extension first, then by content: a file whose extension does not match
its bytes (for example a Transport Stream saved as `.mkv`) is still recognised and parsed
correctly, at no cost to correctly-named files.

**Metadata sidecar files.** These carry no picture data, bypass the video pipeline entirely, and
are rendered through the same report, so text, JSON, and quiet output all work unchanged.

| Input | Extension | Notes |
|---|---|---|
| Dolby Vision RPU | `.bin`, `.rpu` | Raw RPU stream (for example from `dovi_tool extract-rpu`), aggregated across every frame |
| Dolby Vision CM XML | `.xml` | Dolby CM metadata (DolbyLabsMDF), aggregated per shot |
| HDR10+ JSON | `.json` | hdr10plus_tool metadata; reports the file-level profile and the first scene from a bounded head read |

Each sidecar is identified by content rather than extension alone, so an unrelated `.bin`,
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

### Download a prebuilt binary (recommended)

The simplest way to get hdrprobe is to download a ready-to-run binary from the
[latest release](https://github.com/matthane/hdrprobe/releases/latest). Grab the archive for
your platform, unpack it, and run the `hdrprobe` binary. Prebuilt binaries are provided for:

- Windows (x86_64)
- Linux (x86_64, and ARM64 with both a standard and a fully-static build for minimal systems)
- macOS (Apple Silicon and Intel)

The binary is self-contained with no runtime dependencies, so you can drop it anywhere on your
`PATH` and run it. Each release lists a `SHA256SUMS` file if you want to verify your download.

### Build from source

If you would rather build it yourself, you need a Rust toolchain (1.85 or newer):

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

For scripting against the JSON output, every object and field is documented in
[docs/SCHEMA.md](docs/SCHEMA.md).

### Options

| Flag | Effect |
|---|---|
| `-j, --json` | JSON instead of text (an array when several files are given) |
| `--format <fmt>` | `text` (default), `json`, or `ndjson` (one object per line) |
| `-f, --full` | Exhaustive per-frame scan: all distinct L5, a full trim-target census, and scene counts. Trades speed for completeness. Shows a live progress bar in the terminal. |
| `--progress <mode>` | Progress reporting for `--full` scans: `auto` (default, bar on an interactive terminal), `bar`, `json` (machine-readable events on stderr, see [docs/SCHEMA.md](docs/SCHEMA.md)), or `off` |
| `--no-rpu` | Report the container DV configuration only, skipping RPU parsing. Effectively instant. |
| `-s, --samples <N>` | Number of seek points to sample (default 16). Higher values capture more distinct L5 areas. |
| `--sections <list>` | Comma-separated list drawn from `general,hdr,dv,hdr10plus` |
| `--color <when>` | `auto` (default, plain when piped), `always`, or `never` |
| `--theme <name>` | Color theme: `paper` (default), `green`, `amber`, `red`, `ice`, `purple`, or `mono` (adapts to your terminal's own colors). Set `HDRPROBE_THEME` to make it stick. |
| `-q, --quiet` | One-line summary per file |
| `-r, --recursive` | Descend into directory arguments |
| `--threads <N>` | Number of parallel workers (default: logical core count) |
| `-o, --output <path>` | Write to a file instead of stdout |
| `--install-shell` | Windows: add a right-click "hdrprobe" context menu with Fast and Full entries |
| `--uninstall-shell` | Windows: remove that context menu |
| `-h, --help`, `-V, --version` | Standard |

Exit codes: `0` parsed successfully, `1` usage error, `2` unreadable or corrupt input.

### Color themes

`--theme` picks the palette for one run. To make a theme your default everywhere, set the
`HDRPROBE_THEME` environment variable. On Windows:

```sh
setx HDRPROBE_THEME amber
```

This applies to every new terminal window and to the right-click menu entries (already-open
terminals keep their old environment until reopened). On Linux and macOS, add
`export HDRPROBE_THEME=amber` to your shell profile.

An explicit `--theme` still overrides the variable for that run. Themes only affect colored
output: piped, `--quiet`, and JSON output stay plain regardless.

### Windows shell integration

```sh
hdrprobe --install-shell     # register the right-click menu entry
hdrprobe --uninstall-shell   # remove it
```

`--install-shell` adds an "hdrprobe" submenu to the right-click menu of every
supported file type, so any video or metadata sidecar can be inspected from
Explorer. It has two entries: **Fast** runs the normal quick scan, and **Full**
runs the exhaustive `--full` scan of the whole file. Either opens a console
running the report, kept open until you press a key. The menu launches whichever
`hdrprobe.exe` you ran the install from, so run it from the binary's final
location.

Registration is per-user, so it needs no administrator rights: it writes verbs under
`HKCU\Software\Classes\SystemFileAssociations` and touches no default file
associations. On Windows 11 the entry lives under "Show more options" in the
right-click menu.

## Performance

hdrprobe reads only the bytes it needs. It memory-maps the file and samples a spread of seek
points in parallel rather than scanning the whole stream, so cost stays flat regardless of file
size. On remote volumes (SMB and NFS) it warms the regions it is about to parse ahead of time,
so a scan does not stall on hundreds of round-trips. The `--full` flag trades this speed for
completeness, lifting the sampling bounds for an honest whole-stream census. A deep scan of a
file on a network share streams it across the wire once at full speed, instead of thousands of
small reads, with the progress bar pacing the transfer.

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
