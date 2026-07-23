```
█ █ █▀▄ █▀█ █▀█ █▀█ █▀█ █▄▄ █▀▀
█▀█ █▄▀ █▀▄ █▀▀ █▀▄ █▄█ █▄█ ██▄  v0.7.0
```

Fast HDR, HDR10+, and Dolby Vision metadata inspector.

hdrprobe is a single native binary that answers one question quickly: what HDR and dynamic metadata does this file actually carry? It does the work that normally requires
`mediainfo`, `ffprobe`, `dovi_tool`, and `hdr10plus_tool` together, without launching subprocesses in the hot
path, writing temp files, or extracting a full RPU stream to disk first. It memory-maps the
file and reads only the bytes it needs, so it stays fast regardless of file size.

## Quick start

Download the archive for your platform from the
[latest release](https://github.com/matthane/hdrprobe/releases/latest), unpack it, drop the
`hdrprobe` binary anywhere on your `PATH`, and point it at a file:

```sh
hdrprobe movie.mkv
```

The result is a sectioned report of everything the file carries:

```
▮ movie.mkv 68.55 GiB

── GENERAL ───────────────────────────────────────────────────────
  Container         Matroska
  Duration          2h 17m 04s
  Bitrate           66.44 Mb/s · video stream
  Video             HEVC (Main 10, High tier @ L5.1) · 3840×2160 ·
                    23.976 fps · 10-bit 4:2:0
  Color             BT.2020 · PQ (SMPTE ST 2084) · limited

── HDR ───────────────────────────────────────────────────────────
  Format            Dolby Vision / HDR10+ / HDR10 (fallback)
  Mastering         DCI-P3 D65 · max 1000  min 0.0001 cd/m²
  Content light     MaxCLL 737 · MaxFALL 130

── DOLBY VISION ──────────────────────────────────────────────────
  Structure         Single track, dual layer
  Profile           7.6 (FEL)
  Content mapping   v4.0
  Reconstruction    12-bit (10-bit BL + FEL residual)
  Mastering         DCI-P3 D65 L9 · max 1000  min 0.0001 cd/m²
  Trim targets*     100 nits L2/L8, 600 nits L2, 1000 nits L2
  L5 offsets*       L0 R0 T276 B276
  L5 active area    3840×1608  (2.39:1)
  L6 content light  MaxCLL 737 · MaxFALL 130
  L11 APO           Movies · white point D65 · reference mode

── HDR10+ ────────────────────────────────────────────────────────
  Profile           B
  Application       v1
  Windows           1
  Target            400 nits

* sampled from a spread of RPUs; --full reads every one
```

## Contents

- [What it reports](#what-it-reports)
  - [General video info](#general-video-info)
  - [HDR static metadata](#hdr-static-metadata)
  - [Dolby Vision dynamic metadata](#dolby-vision-dynamic-metadata)
  - [HDR10+ dynamic metadata](#hdr10-dynamic-metadata)
  - [SL-HDR dynamic metadata](#sl-hdr-dynamic-metadata)
  - [HDR Vivid dynamic metadata](#hdr-vivid-dynamic-metadata)
  - [Badges and footnotes](#badges-and-footnotes)
- [Supported inputs](#supported-inputs)
- [Install](#install)
- [Usage](#usage)
  - [Options](#options)
  - [Color themes](#color-themes)
  - [Windows shell integration](#windows-shell-integration)
- [Performance](#performance)
- [Trademarks](#trademarks)
- [License](#license)

## What it reports

Everything below is reported per video track. Most files carry a single video track and read
exactly as shown above; when a file carries several (a remux with more than one cut of a title,
or a broadcast capture with one video stream per service), each track gets its own labelled
group in the report, and quiet mode prints one summary line per track. A Dolby Vision
dual-layer pair (base layer plus enhancement layer) is one logical track, not two, whatever
the container layout.

### General video info

- Container, codec, and codec profile
- Resolution, frame rate, bit depth, and chroma subsampling
- Colour signalling: primaries, transfer, matrix, and range
- Stereoscopic / multiview structure (MV-HEVC, such as Dolby Vision Profile 20)

### HDR static metadata

- The static mastering display characteristics: ST.2086 min/max luminance and the mastering
  gamut, named when recognized (BT.2020, DCI-P3 D65, DCI-P3, or BT.709)
- MaxCLL / MaxFALL content light levels

### Dolby Vision dynamic metadata

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

Alongside the profile:

- Track and layer structure, for dual-layer content
- Content-mapping version: `v2.9`, or `v4.0` via L254
- Reconstructed signal bit depth for full-enhancement (FEL) streams: 12-bit on Profile 7,
  14-bit on Profile 4
- The DV grade's own mastering display, which can differ from the base layer's: on a Profile 7
  title a 4000-nit grade can sit over a 1000-nit HDR10 base

**Dynamic levels.** The distinct values seen across the title:

- `L5` offsets and active areas
- `L6` content light
- `L9` mastering gamut
- `L11` APO (content type, white point, and reference flag)
- The set of `L2` / `L8` trim targets

**Deliberately omitted.** The per-frame and per-shot analysis levels (`L1` brightness, `L3` L1
offsets) and the per-shot trim values. These vary shot to shot rather than describing the title,
so they collapse to nothing meaningful once sampled or aggregated. One title-stable fact about
them is reported instead: when every frame is read (a `--full` scan or a sidecar file), a
metadata cadence line says whether the dynamic metadata was authored shot-by-shot (the standard
studio workflow) or frame-by-frame (typical of converted or automatically analysed metadata).

RPU parsing is native and in-process via [`libdovi`](https://github.com/quietvoid/dovi_tool)
(the `dolby_vision` crate); HDR10+ parsing uses the sibling `hdr10plus` crate.

### HDR10+ dynamic metadata

- Presence and profile
- Application version
- Window count
- Target display max luminance

Detected wherever the format is carried: HEVC SEI messages, AV1 metadata OBUs, or the
Matroska/WebM carriage used by VP9.

### SL-HDR dynamic metadata

- Presence and mode: SL-HDR1, SL-HDR2, or SL-HDR3, named on the format line alongside the
  base signal (for example `SL-HDR2 / HDR10`)
- Declared specification version
- Payload carriage: parameter-based or table-based
- The target presentation the adaptation metadata is tuned toward: its colour primaries and
  peak luminance (for example a 100-nit BT.2020 SDR rendition)

SL-HDR is defined by ETSI TS 103 433 and is most common in European broadcast. Detected
from the per-frame carriage in HEVC and AVC streams.

### HDR Vivid dynamic metadata

- Presence, named on the format line alongside the base signal (for example
  `HDR Vivid / HLG`)
- Declared metadata version
- The set of display targets the tone-mapping metadata is authored for (for example
  100 nits alongside 500 nits)

HDR Vivid is the UWA (CUVA) standard used in Chinese broadcast and streaming. Detected from
the per-frame carriage in HEVC and AV1 streams, and from MP4 container signalling.

### Badges and footnotes

Some lines carry a highlighted badge when the metadata shows something worth a second look:

| Badge | Appears on | Meaning |
|---|---|---|
| `variable` | L5 offsets | More than one distinct active area: the picture's aspect ratio changes across the title |
| `zeroed` | Content light, L6 content light | MaxCLL / MaxFALL are signalled but both zero, a placeholder left in by the authoring tool (a common real-world defect) |
| `MDP mismatch` | DV Mastering | The Dolby Vision grade's mastering gamut (L9) disagrees with the base layer's own signalled mastering display primaries, usually drift left behind by a re-encode |
| `FEL brightness expansion` | DV Mastering | The Dolby Vision grade's mastering display is brighter than the one declared for the base layer (for example a 4000-nit grade over a 1000-nit HDR10 base): the base layer is a tone-mapped rendition of a brighter master, and the full-enhancement layer's residual is what restores those highlights, so stripping it (a Profile 7 to 8 conversion) would discard them |
| `Unconverted RPU` | DV Profile | The RPU still carries the dual-layer composer metadata of its source (typically a UHD Blu-ray Profile 7 title), but this stream has no enhancement layer: the RPU was carried into a single-layer transcode without being converted first. Playback is unaffected, but tools that guess a profile from the RPU can be misled by the leftover metadata; the out-of-spec `10.6` some AV1 transcodes declare is exactly this. Converting the RPU (for example with `dovi_tool`) before the encode avoids it |

These are observations about the metadata, not errors; the file still plays. They surface
inconsistencies a normal player silently ignores but a remuxer or encoder may care about.

A row label can also carry a footnote mark (`*`, `†`), spelled out once at the foot of the
report:

- **Sampled.** By default, RPU-derived sets (trim targets, L5 offsets and active areas) come
  from a spread of sample points, so the set may be incomplete. `--full` reads every RPU and
  drops the mark.
- **Assumed canvas.** Dolby Vision sidecar files carry no resolution, so their L5 active areas
  are computed against an assumed 3840×2160 master.

## Supported inputs

hdrprobe reads both video files and standalone metadata sidecar files:

| Input | Type | Codecs | Notes |
|---|---|---|---|
| MP4 / MOV | Video | HEVC, AVC, AV1, VP9, ProRes | One or more video tracks; an enhancement layer may ride its own track |
| MKV / WebM | Video | HEVC, AVC, AV1, VP9, ProRes | One or more video tracks; an enhancement layer is typically interleaved into its base track |
| MPEG-TS / M2TS | Video | HEVC, AVC | One or more programs, each with its own video stream; an enhancement layer may ride its own PID |
| Blu-ray ISO (`.iso`) | Video | HEVC, AVC | Decrypted disc image: hdrprobe reads the disc's playlists, picks the main feature automatically, and reports on it as if the stream file had been probed directly. Encrypted images are detected and rejected |
| Raw HEVC (Annex-B) | Video | HEVC | Elementary stream; profile inferred from the RPU |
| Raw AV1 (IVF or low-overhead OBU) | Video | AV1 | Elementary stream; the RPU rides an in-band metadata OBU |
| Raw VP9 (IVF) | Video | VP9 | Elementary stream; a bare VP9 stream carries no HDR signalling of its own, so colour beyond matrix and range comes only from a container |
| Dolby Vision RPU (`.bin`, `.rpu`) | Sidecar | – | Raw RPU stream (for example from `dovi_tool extract-rpu`), aggregated across every frame |
| Dolby Vision CM XML (`.xml`) | Sidecar | – | Dolby CM metadata (DolbyLabsMDF), aggregated per shot |
| HDR10+ JSON (`.json`) | Sidecar | – | hdr10plus_tool metadata; reports the file-level profile and the first scene from a bounded head read |

Inputs are matched by extension first, then by content: a file whose extension does not match
its bytes (for example a Transport Stream saved as `.mkv`) is still recognised and parsed
correctly, at no cost to correctly-named files. Likewise each sidecar is identified by content
rather than extension alone, so an unrelated `.bin`, `.xml`, or `.json` in a scanned directory
is skipped.

Sidecars carry no picture data, bypass the video pipeline entirely. Because none of these
formats records a resolution, the L5 active-area dimensions for the Dolby Vision sidecars are
computed against an assumed UHD (3840x2160) master and labelled as assumed in the report.

A note on WebM: WebM is a subset of Matroska, parsed by the same backend, whose codec
whitelist excludes HEVC and with it the common Dolby Vision profiles; the only one it can
carry is AV1's Profile 10, which hdrprobe reports correctly (the RPU rides in-band). WebM's
native HDR codec is instead VP9, typically 10-bit Profile 2 with the HDR signalling carried
by the container, including the mastering display, content light levels, and, when present,
per-frame HDR10+ metadata. hdrprobe reads all of it, from WebM and MKV alike, and reports
the same sections an HEVC or AV1 HDR file gets.

## Install

### Download a prebuilt binary (recommended)

The simplest way to get hdrprobe is to download a ready-to-run binary from the
[latest release](https://github.com/matthane/hdrprobe/releases/latest). Grab the archive for
your platform, unpack it, and run the `hdrprobe` binary. Prebuilt binaries are provided for:

- Windows (x86_64)
- Linux (x86_64 and ARM64, each with a standard build and a fully-static build for minimal
  systems such as Unraid or CoreELEC boxes)
- macOS (Apple Silicon and Intel)
- FreeBSD (x86_64)

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
hdrprobe --json movie.mp4 | jq '.video_tracks[].dolby_vision.profile'
hdrprobe --full movie.mkv              # exhaustive per-frame scan and census
hdrprobe --no-rpu disc.m2ts            # container-level DV config only
hdrprobe RPU.bin                       # inspect a raw Dolby Vision RPU sidecar
hdrprobe metadata.json                 # inspect an HDR10+ JSON sidecar
hdrprobe --format ndjson -r ./library > report.ndjson
curl -sr 0-25165823 "$URL" | hdrprobe --json -   # probe the head of a piped stream
```

A directory argument is scanned for video files. Add `-r` to descend into subdirectories.

Media that has no file path (a network stream, or an app's internal virtual file system) can
be piped to `hdrprobe -`: it probes the beginning of the stream, takes only what it needs, and
the report says when it saw a partial stream. Integrators can start with
[docs/INTEGRATION-STDIN.md](docs/INTEGRATION-STDIN.md).

For scripting against the JSON output, every object and field is documented in
[docs/SCHEMA.md](docs/SCHEMA.md).

### Options

| Flag | Effect |
|---|---|
| `-j, --json` | JSON instead of text (an array when several files are given) |
| `--format <fmt>` | `text` (default), `json`, or `ndjson` (one object per line) |
| `-f, --full` | Exhaustive per-frame scan: all distinct L5, a full trim-target census, scene counts, and the shot-based vs frame-by-frame metadata cadence. Trades speed for completeness. Shows a live progress bar in the terminal. |
| `--progress <mode>` | Progress reporting for `--full` scans: `auto` (default, bar on an interactive terminal), `bar`, `json` (machine-readable events on stderr, see [docs/SCHEMA.md](docs/SCHEMA.md)), or `off` |
| `--no-rpu` | Report the container DV configuration only, skipping RPU parsing. Effectively instant. |
| `-s, --samples <N>` | Number of seek points to sample (default 16). Higher values capture more distinct L5 areas. |
| `--sections <list>` | Comma-separated list drawn from `general,hdr,dv,hdr10plus,slhdr,hdrvivid` |
| `--color <when>` | `auto` (default, plain when piped), `always`, or `never` |
| `--theme <name>` | Color theme: `paper` (default), `green`, `amber`, `red`, `ice`, `purple`, or `mono` (adapts to your terminal's own colors). Set `HDRPROBE_THEME` to make it stick. |
| `-q, --quiet` | One-line summary per file |
| `-r, --recursive` | Descend into directory arguments |
| `--threads <N>` | Number of parallel workers (default: logical core count) |
| `-o, --output <path>` | Write to a file instead of stdout |
| `--install-shell` | Windows: add a right-click "hdrprobe" context menu (files and folders) with Fast and Full entries |
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
supported file type and of folders, so any video, metadata sidecar, or whole
directory can be inspected from Explorer. It has two entries: **Fast** runs the
normal quick scan, and **Full** runs the exhaustive `--full` scan of the whole
file. On a folder, either entry scans every supported file in it, including
subfolders. Either opens a console running the report, kept open until you
press a key. The menu launches whichever `hdrprobe.exe` you ran the install
from, so run it from the binary's final location.

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
HDR10+ Technologies, LLC. SL-HDR is a standard developed by Philips, Technicolor, and
InterDigital and standardized by ETSI. HDR Vivid is a standard developed by the China Ultra HD
Video Industry Alliance (CUVA). All other product names, logos, and brands are the property of
their respective owners.

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
