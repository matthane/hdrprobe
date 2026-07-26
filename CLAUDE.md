# CLAUDE.md — working notes for hdrprobe

Fast HDR / HDR10+ / Dolby Vision metadata inspector: one native Rust binary that memory-maps a video
file, demuxes without decoding, samples RPUs, and prints a sectioned report in less than 1 second.
It also parses metadata **sidecar** files (raw DV RPU, DV CM XML, HDR10+ JSON) into the same
report. This file plus the module-level doc comments are the design reference — read the
relevant section and the code it points at before non-trivial changes.

## Commands

```sh
cargo build --release          # binary at target/release/hdrprobe
cargo test                     # 451 unit tests
cargo clippy --release         # must stay at zero warnings
./target/release/hdrprobe testfiles/integration/ -q   # one-line report per corpus file
```

Bar for any change: **zero `cargo build` warnings, zero `cargo clippy` warnings, all tests
pass, and the corpus (`-q`) output is unchanged** unless the change intends to alter it.

## Branch flow

Each version cycle develops on its own branch (convention: `dev/vX.Y.Z`, named for the
upcoming version). **Never commit work-in-progress directly to main**: main receives the
cycle as a single `--no-ff` merge at release time, so pre-release doc/schema edits (README,
SCHEMA.md "Ships in" notes) never appear on main ahead of the version they describe. The
project-local `/commit` skill pushes to the development branch only; `/release` performs
the merge, tag, and push to main. (History before v0.8.0 was committed straight to main.)

## Third-party license attribution

`THIRD-PARTY-LICENSES.md` is **generated — never hand-edit it.** It lists every crate compiled
into the release binary, grouped by license, and is produced by [`cargo about`](https://github.com/EmbarkStudios/cargo-about)
from `about.toml` (the accepted-license allowlist + the target set we publish for) and `about.hbs`
(the Markdown template):

```sh
cargo install --locked --features cli cargo-about     # one-time
cargo about generate about.hbs -o THIRD-PARTY-LICENSES.md
```

The committed file must match what the current dependency tree produces, so after any change to
`Cargo.toml`/`Cargo.lock` — and as a **release gate** — regenerate and fail on drift:

```sh
cargo about generate about.hbs -o THIRD-PARTY-LICENSES.md
git diff --exit-code THIRD-PARTY-LICENSES.md          # nonzero exit => stale; commit the update
```

Generation itself **fails** if a dependency pulls in a license not in `about.toml`'s `accepted`
list — that's the guard against silently bundling an incompatible (e.g. copyleft) license into a
binary, not a nuisance: vet the license and confirm MIT-compatibility before adding it. The project
itself stays MIT (see `LICENSE`); dev-dependencies (not shipped) and the `hdrprobe` crate itself are
excluded by config.

## Release binaries

Pushing a version tag (`v*`) runs `.github/workflows/release.yml`: it enforces the gates above
(clippy/tests under `-Dwarnings`, the license drift check, tag == `Cargo.toml` version), builds and
tests the binary for Windows x86_64, Linux x86_64 + aarch64 each as glibc + fully-static musl
(no libc/loader dependency — aarch64 for minimal userspaces like CoreELEC/LibreELEC boxes across
old vendor kernels through current, x86_64 for appliance NAS OSes like Unraid whose RAM-resident
root makes a drop-in static binary the natural install), macOS arm64 + Intel (Intel is
cross-compiled on the arm64 runner and tested via Rosetta), and FreeBSD x86_64 (no GitHub
runner exists, so a separate job builds, tests, and packages inside a FreeBSD VM on the Linux
runner — keeping the build-and-test-on-target rule), and attaches the archives plus
`SHA256SUMS` to a **draft** GitHub release for manual review. Archives are named by
user-facing platform (`hdrprobe-<version>-linux-x64-static` etc., the matrix `name`
field), never raw target triples — keep that mapping when adding targets. A `workflow_dispatch` run exercises
the gates and builds without creating a release. The corpus `-q` check stays a manual pre-tag step
(`testfiles/` is local-only). The code is deliberately portable outside `shell.rs`/`prefetch.rs`'s
`cfg(windows)` branches — keep new platform-specific code behind `cfg` with a non-Windows path, and
never parse bytes native-endian.

- `main.rs` — clap CLI, per-file dispatch (sidecar files first, then the Blu-ray ISO branch,
  then the video pipeline), exit codes (0 ok / 1 usage / 2 unreadable).
- `container/` — one hand-rolled demuxer per format: `mp4.rs`, `mkv.rs`, `ts.rs`, `annexb.rs`,
  `av1.rs` (which also owns the IVF wrapper's FourCC dispatch: `VP90` → the VP9 IVF demux,
  `VP80` → an honest error, else AV1), `mpegv.rs` (raw MPEG-1/2 video elementary stream, the
  thinnest backend in the tree: a bounded head read fills the General fields and `chunks` stays
  empty, per the metadata-only contract on `TrackDemux::chunks`; under `--full` a count-only
  fused walk — `RawFullStream::Mpegv`, the Ogg shape — counts picture start codes so duration
  is frames ÷ the sequence header's rate and the bitrate is the file's bytes over it, at
  `video_stream` scope, reproducing MediaInfo byte-exactly on both corpus raw streams), `ps.rs` (below); `mod.rs` holds
  `Demux`/`Chunk`/`DvConfig`, the shared dvcC/hvcC/CICP decoders, `fill_nal_config_fields` (an
  `avcC`/`hvcC` record to a `TrackDemux`, shared because AVI, ASF and FLV all hand one over and
  each reaches it through a different carriage's framing test), and — since TS and PS both
  reassemble an elementary stream out of packet payloads and then have to read the picture out
  of the bitstream — the shared in-band SPS search (`SpsCommon`, `best_sps`, `sps_fields`).
- `hevc/` — `nal.rs` (Annex-B + length-prefixed NAL split), `sps.rs` (dims + VUI colour + VUI
  timing/frame rate).
- `avc/` — the H.264 analogue, for Dolby Vision **Profile 9** (`dvav.09`: 8-bit AVC, single-layer,
  SDR-compatible Rec.709 base). `nal.rs` (1-byte NAL header — `nal_type = byte & 0x1F`), `sps.rs`
  (macroblock-based dims + the profile_idc-gated high-profile chroma/depth block + VUI colour + VUI
  timing). Reuses `hevc::sps::VuiColor` so the shared `container::color_from_vui` plumbing is
  unchanged.
- `av1/` — `obu.rs` (OBU walker, T.35 routing), `seq.rs` (sequence header).
- `vp9.rs` — the VP9 analogue: keyframe uncompressed-header parse (profile/depth/chroma +
  matrix/range — the header names **no transfer or primaries**, so a bare VP9 stream can never
  classify HDR; container colour keeps authority, the header only fills gaps), the WebM
  CodecPrivate feature list (optional — mkvmerge wrote none before ~v30; `mkv.rs` falls back to
  the first keyframe via `fill_vp9_stream_fields`), and `profile_label`. VP9 has no in-band
  SEI/RPU: its HDR10+ rides MKV `BlockAdditions` with `BlockAddID == 4` (a raw ITU-T T.35
  message, recorded per track as `TrackDemux::t35_chunks` — the DV-EL addition slot, ID 1,
  stays ignored) and `sample.rs` merges those payloads through the same `sei::parse_hdr10plus`
  gate the AV1 T.35 route uses. MP4 carries VP9 as `vp09` + `vpcC` (CICP + range directly in
  the record — parsed in `mp4.rs`).
- `prores.rs` — the ProRes analogue, plainer still: the frame header (every frame is
  intra-coded and carries one) gives chroma format and its own CICP colour bytes, which real
  encodes routinely leave unspecified (the corpus MKV says 2/2/6 under real BT.2020/PQ
  container signalling) — container colour keeps authority, the header only fills gaps, and
  bit depth is the profile family's defined depth (4:2:2 → 10, 4444 → 12; the header has no
  depth field). ProRes has **no bitstream side channel at all** — no SEI/RPU/T.35; static HDR
  rides MKV `Colour` / MP4 `colr`+`mdcv`+`clli`, and DV masters pair with CM XML sidecars —
  so the sampler's ProRes arm is a deliberate no-op. The profile is signalled **only** by the
  MOV/MP4 sample-entry FourCC (`apco/apcs/apcn/apch/ap4h/ap4x` → `profile_from_fourcc`);
  Matroska's `V_PRORES` carries no FourCC and a void CodecPrivate, and its blocks strip the
  frame's 8-byte `size+'icpf'` atom header (`parse_frame_header` accepts both forms), so an
  MKV mux reports no profile (MediaInfo/ffprobe agree — never guess). Both backends run the
  shared `container::fill_prores_stream_fields` over the first frame: MKV for depth/chroma
  (stated nowhere else), and both for the colour gap-fill — an ffmpeg-written ProRes MOV
  carries no `colr` box at all, leaving the frame header's CICP as the only colour signal
  (verified: without the fill such a PQ master classifies SDR). ProRes RAW (`aprn`/`aprh`)
  is a different codec family and stays on the `Other` fallback.
- `mpeg2.rs` — MPEG-1 (ISO/IEC 11172-2) and MPEG-2 (ITU-T H.262 | ISO/IEC 13818-2) video, read
  from the `sequence_header` and its two sequence extensions through the same gap-filler shape
  (`container::fill_mpeg2_stream_fields`). The module doc carries the carriage details; four
  facts are invariants a later change would otherwise undo silently, each pinned by a test naming
  its spec table. **Colour has no defaults**: H.262 leaves an absent `sequence_display_extension`
  "implicitly defined by the application", so nothing is filled and the fields stay genuinely
  unsignalled (D4 of `dev/sdr-coverage-plan.md`); filling BT.601 would fabricate. **Colour value
  0 is Forbidden, not CICP 0**, so a stray 0 reads as unsignalled rather than decoding to
  "RGB"/"Identity". **Bit depth is not signalled** and is the spec constant 8 for every defined
  profile, like ProRes's family depth; `intra_dc_precision` is *not* a depth field (values 8..11,
  DC inverse-quantisation scaling) and reporting it would print "11-bit" on an 8-bit stream. And
  **frame-rate codes 9..15 are reserved**, yielding `None`; ffmpeg's `ff_mpeg12_frame_rate_tab`
  fills 9..13 with Xing and libmpeg3 economy rates, so mirroring it invents a frame rate. Codec
  identity in a raw stream comes from the *absence* of a `sequence_extension`, which is what makes
  a stream 11172-2; in a container the container says. Two carriage gates are load-bearing
  elsewhere: the TS descriptor-less multi-PID merge is **HEVC-only** (it is the Dolby Vision
  Profile 7 shape, and MPEG has no enhancement layer, so two MPEG-2 video PIDs are two tracks),
  and MPEG groups are excluded from `ts::sps_rescue` (no SPS exists to find, and hunting one runs
  the walk to EOF, making `--full` two passes over the file).
- `mpeg4part2.rs` — ISO/IEC 14496-2 "MPEG-4 Visual" (Xvid, DivX 4/5/6, 3ivx), read from the
  VOS/VisualObject/VideoObjectLayer headers through the same gap-filler shape
  (`container::fill_mpeg4part2_stream_fields`, which tries the container's own copy of those
  headers before touching a chunk — Matroska CodecPrivate, an `esds` DecoderSpecificInfo, a
  `BITMAPINFOHEADER`'s extradata all hold them verbatim). Three facts are invariants.
  **Colour has spec-defined defaults, unlike MPEG-2**: an absent `video_signal_type()` or a
  clear `colour_description` means BT.709 primaries, transfer *and* matrix plus limited range,
  a genuine `ColorSource::Spec` fill — and the range bit sits one level above the
  `colour_description` flag, so the two halves carry different provenance and are built
  separately. **The frame rate is absent unless `fixed_vop_rate` is set**, which ffmpeg's
  encoder never sets; reporting `vop_time_increment_resolution` instead (what ffmpeg's
  *decoder* surfaces) prints "30000 fps" on ordinary content. And **every marker bit on the
  path is checked rather than skipped** — the VOL is bit-packed with no byte alignment, one
  missed conditional desynchronises every field after it, and the markers are the only
  structural evidence the walk is still aligned. The parser is not a sniffer: `00 00 01 B3` is
  an MPEG-4 group-of-VOP header *and* an MPEG-1/2 sequence header, so it runs only on bytes a
  container already identified as Part 2. The studio VOL layout is sourced to ffmpeg alone and
  says so at the code site (it is the only Part 2 variant signalling a depth above 8 or a
  chroma format other than 4:2:0), and a sprite-coded layer stops the walk rather than guessing
  a block this project has no primary source for.
- `vc1.rs` — SMPTE ST 421 (VC-1). Read against **ST 421:2013**, not the freely circulating
  "VC-9" Committee Draft, whose sequence header has a different layout. Two facts dominate.
  **VC-1's colour fields are not CICP**: the value spaces are narrower, the defaults are
  *not* all 1 (`MATRIX_COEF` defaults to **6**, BT.601, over BT.709 primaries and transfer —
  a mixed combination, and the common case, since every real VC-1 file observed clears
  `COLOR_FORMAT_FLAG`), and **`TRANSFER_CHAR` 8 is BT.1361 where CICP 8 is Linear**, so the
  module translates into CICP codes rather than passing them through. ffmpeg's whitelist both
  drops defined values and admits reserved ones, and GStreamer copies it; the published tables
  win. **And there are two profile numberings.** The in-band sequence header's `PROFILE` is
  2 bits, 0/1/3 for Simple/Main/Advanced. Everything in SMPTE RP 2025-2007's `dvc1` box is
  4 bits, 0/4/12 — §8.1 for `VC1DecSpecStruc.profile`, and §8.3 says `VC1SequenceHeader_C`
  (STRUCT_C) "shall be set to the same value", so **STRUCT_C inside a `dvc1` uses 0/4/12 too**,
  which reads like the bitstream field and is not. `profile_from_config` is the one place that
  numbering lives. VC-1 is also the only codec in this corner of the tree that **does** use
  emulation prevention, so the payload between start codes is unescaped before any bit is read.
- `theora.rs` — Theora (Xiph.Org), read from the 42-byte identification header that sits alone in
  its logical stream's first page, so the whole parse is one page. Ogg records no colour, no
  dimensions and no frame rate of its own, which makes this header the *only* source for every
  picture fact the report carries — a gap-filler with no gap. **`CS` is not CICP**: three values
  (0 undefined, 1 Rec.470M, 2 Rec.470BG) whose primaries and matrix map onto code points, and whose
  **transfer fills as CICP 1 by the field's own definition** — H.273's `transfer_characteristics`
  is the source's opto-electronic function, which Theora fixes at Rec.709's curve for both colour
  spaces; the Rec.470 *display* gammas it also states (2.2 and 2.67) are EOTF-side facts no SDR
  CICP code carries, so they are not evidence against the fill. ffprobe reads the field to the same
  value (bt709 for both defined codes) and MediaInfo reports no Theora colour at all. Plan decision
  D8 originally shipped the transfer unset as the open question's reversible answer; settled and
  reversed with sign-off 2026-07-26. The reserved values above 2 still fill nothing at all, since
  passing them through as CICP would read 5 as "BT.601 (PAL)" and 9 as "BT.2020". CS 1 takes
  matrix code **6** and CS 2 code
  **5** — numerically identical coefficients, so only the label is at stake, and each takes the one
  naming the system its own primaries name. **Display size is not coded size**: `FMBW`/`FMBH` count macroblocks, so an
  854-wide video is coded 864 wide, and `PICW`/`PICH` are used only within 16 pixels of the coded
  size — the spec's own construction rule and ffmpeg's guard. **The granule position is two fields,
  not a shift**: `KFGSHIFT` splits it into a keyframe index and an offset, and the frame count is
  their *sum*, with streams older than 3.2.1 storing the index rather than the count (libtheora's
  own `th_granule_frame` adjustment). Bit depth is the spec constant 8 (§1.2 says wider "is not
  planned"), like MPEG-2's and ProRes's family depth; `NOMBR` is a stated hint and is never a
  bitrate (MediaInfo reports it as one — 200000 on every corpus file against a measured 198868).
- `mjpeg.rs` — Motion JPEG (ITU-T T.81), the plainest gap-filler: depth and chroma from the
  first frame's own `SOF` marker segment, reached by a bounded declared-length marker walk that
  stops at `SOS` (entropy-coded data would otherwise read as markers). Two facts are invariants,
  both pinned by tests. **Depth and chroma are signalled, not family constants** — `SOF`
  precision is 8 only for baseline, and real capture hardware writes 4:2:2 — so they are read,
  never assumed (unlike the WMV/MS-MPEG-4 constants in `container::fill_constant_depth_chroma`).
  And **subsampling is the ratio of luma to chroma factors, not the factors themselves**:
  ffmpeg's 4:4:4 encodes write a uniform `0x12` on all three components, which a raw-factor
  table misreads as nothing. MJPEG records no colour anywhere this tree reads (JFIF's BT.601
  full-range is an interchange-format definition, not a signal), so the Color line stays empty.
  Carriages: VfW `MJPG` (AVI/ASF/MKV — ASF indexes no payload, so its MJPEG reports no
  depth/chroma, honestly), Matroska `V_MJPEG`, QuickTime `jpeg`, MP4 `esds` OTI `0x6C`. Apple's
  `mjpa`/`mjpb` field-split variants and the vendor VfW tags (`dmb1`, `AVRn`, `LJPG`) alter the
  frame layout or lack a witness and stay on the honest-FourCC fallback.
- `container/bmih.rs` — `BITMAPINFOHEADER`, the Video for Windows description block, plus the
  FourCC-to-codec table. It lives in `container/` rather than a backend because three carriages
  hand one over: AVI's `strf`, ASF's type-specific data, and **Matroska's `V_MS/VFW/FOURCC`**,
  which is the only carriage VC-1 has in an MKV. Two traps: **`biBitCount` is display bits per
  pixel and never a bit depth** (a 4:2:0 8-bit stream routinely writes 24), so the field is not
  exposed at all; and **`biHeight` is signed**, negative meaning top-down, so the absolute value
  is the height (`unsigned_abs`, because `i32::MIN.abs()` panics). Extradata is bounded by the
  *caller's* slice, never by `biSize`, which real muxers write inconsistently — the corpus VC-1
  MKV declares 71 over a 72-byte CodecPrivate. The FourCC table names AVC and HEVC, so **every
  caller must pair it with `is_config_record`**: a VfW-wrapped H.264 is length-prefixed with an
  `avcC` in the extradata in one common mux and raw Annex-B in another, the FourCC does not
  separate them (`ffmpeg -c:v copy` from MP4 writes `avc1`, a fresh encode writes `H264`), and
  handing the sampler the wrong framing feeds it bytes that are not NAL units. The test is the
  first extradata byte — a configuration record opens with `configurationVersion == 1`, Annex-B
  with a start code's `0x00` — which is ffmpeg's own discriminator. The length prefix size is
  *not* read there: it lives at a different offset in each record (byte 4 of an `avcC`, byte 21
  of an `hvcC`), so it comes from `parse_avcc_record`/`parse_hvcc_record`, which both MKV arms
  and the AVI backend reach through `mkv::nal_config`'s shape.
- `container/avi.rs` — AVI (RIFF / OpenDML), `.avi`. Everything is a `ckID`/`ckSize` pair, so
  every position is arithmetic on declared sizes and the default path **scans nothing**: the
  `hdrl` is structurally at the head and both index forms are seek-addressable from it (a 1.07 GiB
  two-segment file reports in 29 ms). Six facts are invariants, each pinned by a test.
  **The frame-rate signal is a stream *unit* rate, not a picture rate** — `strh.dwRate/dwScale`
  counts whatever `dwLength` counts, and an `ffmpeg -i x.mp4 -c:v copy out.avi` remux (a very
  common operation) declares 1200 units at 600/1 for a 2 s 25 fps clip, 1150 of them zero-length
  padding chunks. MediaInfo reports 600.000 fps for it. So the *coded stream's* own rate wins
  where it has one and the container's is the fallback, which is why `demux` fills `fps` only
  `if td.fps.is_none()` after the bitstream parsers have run; the duration is unaffected either
  way, since both halves of `dwLength / (dwRate/dwScale)` count the same units.
  **`avih.dwTotalFrames` is wrong on every OpenDML file** and is never read; `strh.dwLength` is
  the whole-file count on both ffmpeg and VirtualDub. **The `odml` LIST is written as a `JUNK`
  chunk with a zero frame count on a single-RIFF file** — ffmpeg reserves the block and rewrites
  the id to `LIST` only on rollover, so all ten corpus AVIs (including the one *named*
  `h264_odml.avi`) carry a `JUNK`-wrapped `dmlh` reading 0 while a real two-segment file reads
  5000. `dmlh` must therefore be reached by descending a genuine `LIST odml`, never by searching
  for its FourCC. **The two index forms use opposite offset conventions**: `idx1` entries address
  the chunk *header* relative to the `movi` FOURCC (so entry 0 reads 4), while OpenDML `ix##`
  entries address the chunk *data* relative to `qwBaseOffset`; both bases are derived and then
  *validated* against the chunk actually at that position, because Microsoft documents an
  absolute-offset `idx1` variant (VirtualDub before build 4936) and nothing else would catch a
  misread. **`idx1` covers only the first RIFF segment**, a 22% byte undercount on a real
  two-segment file, so summing it is gated on the file being single-segment; multi-segment files
  sum the `ix##` chunks the `indx` super-index points at. **`idx1` entry 0 need not be video** —
  it is `01wb` on the corpus's video+MP3 file — so entries are filtered by the two-digit
  stream-index prefix and the base comes from entry 0's own chunk id (unfiltered, that file's
  rate is 12.5% high). Deliberately not parsed: `vprp` (it carries only display aspect ratio and
  field order, both out of scope plan-wide, plus a refresh rate and geometry that duplicate
  `strh`/`strf`), `dwMaxBytesPerSec` (a whole-file maximum including audio) and `biBitCount`.
- `container/asf.rs` — ASF / Windows Media (`.wmv`, `.asf`, and a `.wma` carrying video), a flat
  tree of `GUID` + `u64 size` objects walked from byte 0. The Header Object declares its own size
  and holds every reported field, so the parse is a bounded head read and **`chunks` stays empty
  by design**, not by omission: ASF Data Packets carry a length-encoded payload header with
  several payloads per packet and payloads split across packets, so a video access unit is not a
  byte range in the file, and nothing in the report needs one (VC-1 publishes its sequence header
  into codec-private data by every carriage spec that defines one, the Windows Media codecs signal
  nothing in band, and the bitrate and duration are stated outright). Five facts are invariants.
  **GUIDs are mixed-endian and are generated, never hand-typed** (`guid()` does
  `struct.pack('<IHH', d1, d2, d3) + bytes[8:16]` at compile time) — the reference records three
  being mistyped while it was written, each reporting an object as absent from a file that plainly
  contains it. **The tree must be walked, never searched**: real files are multi-stream and the
  audio stream is routinely first, so the first `Stream Properties Object` is the wrong one, and
  there is one `Extended Stream Properties Object` *per stream*, correlated by `Stream Number`.
  **Play Duration is offset by Preroll and the units differ** (100-nanosecond ticks against
  milliseconds); Microsoft's encoder writes a 5000 ms preroll and ffmpeg 3100 ms, so skipping the
  subtraction overstates a 2-second clip by 155%. **`Maximum Bitrate` is a whole-file ceiling and
  is never read**; the per-video-stream rate is the ESP `Data Bitrate` (the leak rate "excluding
  all ASF Data Packet overhead"), with `Stream Bitrate Properties` as the ~1%-higher fallback that
  the spec says *should* include that overhead — MediaInfo prefers the same one. And **ASF records
  no colour anywhere**, so a WVC1 track's colour comes from the VC-1 sequence header in the
  extradata and `WMV1`/`WMV2`/`WMV3` correctly report none. Two smaller traps: the object walk is
  *recursive* (into the Header Extension) and a Header Extension costs 46 bytes, so it is
  depth-bounded — a stack overflow is an abort, outside the 0/1/2 exit contract and outside
  `catch_unwind`; and the WVC1 extradata does **not** begin at its start code (both
  Microsoft-authored corpus files write one leading byte before `00 00 01 0F`), which is why
  `vc1::parse_sequence_header` locates the EBDU instead of reading from offset 0, exactly as
  ffmpeg's `vc1_decode_init` does.
- `container/flv.rs` — FLV (Adobe Flash Video) and Enhanced FLV / E-RTMP, `.flv`: a 9-byte header
  then a flat chain of `11-byte header + DataSize payload + 4-byte back-pointer` tags, so the walk
  is declared-size arithmetic like AVI's, without an index. **Big-endian throughout**, the
  opposite of every other byte-oriented container here. Seven facts are invariants.
  **`TimestampExtended` is the high byte** (`(byte 7 << 24) | u24(bytes 1..4)`), not a fourth low
  byte, and **`TagType` is `byte & 0x1F`** (the top bits are reserved plus the `Filter` encrypted
  flag, whose payload must not be handed to a parser). **`onMetaData` is a hint**: FLV's habitat is
  RTMP ingest, its `duration` is routinely absent or zero, and ffmpeg gates width/height/codec from
  it behind a `trust_metadata` option — so the coded stream wins every field it states and the
  metadata fills what is left. Its `filesize`, when declared, is what tells a truncated file from a
  complete one. **The ecma-array count is documented as approximate and must never bound the
  parse** — writers emit 0 over a populated array — so the property list ends on its `00 00 09`
  terminator with the tag's `DataSize` as the outer bound, and the AMF reader additionally carries
  a *node budget*, because a property list costs three bytes per entry while each entry allocates a
  `String`: that product is the bounded quantity, the same shape as the AVI index defect.
  **`videodatarate` is in units of 1024 bits/s, not 1000** (7812.5 and 29296.875 on the two real
  corpus files recover exactly 8 and 30 Mbit/s; x1000 is 2.4% low and not round), and it is
  muxer-declared rather than measured, which is why a `--full` walk's exact sum replaces it.
  **The Enhanced header must be detected before the CodecID is read** (bit 7 of the first payload
  byte), a **`ModEx` packet type prefixes a variable-length block and re-states the type** so the
  FourCC is not at a fixed offset, and **the 3-byte composition time is present only on
  `CodedFrames` (1) *and* only for `avc1`/`hvc1`/`vvc1`** — `CodedFramesX` (3) never has it and
  `av01`/`vp08`/`vp09` never have it, so a fixed payload start shifts an access unit by three
  bytes, which for AV1 lands past the metadata OBUs that open the unit and costs the entire
  dynamic report (RPU, HDR10+ T.35, CLL/MDCV). Reference §6 describes packet type 1 as "with s24
  CTS" without the codec gate and is wrong. A **`videoFrameType` of 5 (Command) carries a command
  byte where the FourCC would be**, except inside a Metadata packet — which is exactly the
  combination the corpus's own `colorInfo` tag uses, so the two conditions must be tested
  together. And **Enhanced `colorInfo` luminance is in nits where ST.2086's is 0.0001 cd/m²**, a
  deliberate departure the E-RTMP spec calls out; reading it as ST.2086 misreports min luminance by
  10000x. That last one has **no real-bytes fixture** — ffmpeg's muxer writes only
  `colorConfig.matrixCoefficients`, by two independent routes — so it is spec-derived and pinned by
  a unit test alone. Unlike ASF a video access unit *is* a byte range, so the chunk index is real
  and the sampler runs: the corpus's Enhanced FLV reports a mastering display and MaxCLL that exist
  only in the HEVC bitstream's SEI messages. E-RTMP **multitrack** (packet type 6) is detected and
  confined — the first track's FourCC still names the codec, and nothing from such a tag is
  indexed, because what follows the FourCC depends on the multitrack type and blending several
  tracks' access units would be worse than reporting fewer facts. A `vp08`/`vp09` `SequenceStart`
  body is a `VPCodecConfigurationRecord` — the same bytes MP4 puts in a `vpcC` — and reaching it
  is not optional: VP9's bitstream names no transfer and no primaries at all, so that record is
  the only place such a track's colour exists and a BT.2020/PQ stream would otherwise classify
  SDR with nothing able to correct it (hence `container::parse_vpcc_record`, shared with MP4).
  A file whose tag chain names no codec anywhere falls back to `onMetaData.videocodecid` — a
  legacy id as a small integer, an Enhanced FourCC as a big-endian `u32` — which covers an
  encrypted chain, whose `onMetaData` the spec guarantees stays in the clear; with neither, the
  file is refused rather than reporting a placeholder track.
  Three bounding rules earn their own mention because each was a live defect.
  **Every read in a tag header is bounded by the tag, not the buffer**: a `DataSize` of 0 puts the
  video header exactly on the following 4-byte `PreviousTagSize`, and a *well-formed* empty video
  tag writes `00 00 00 0B` there — read as legacy codec id 0. Since the codec gate is first-wins
  and `Codec::Other` has no sampler arm, one such tag makes every later real video tag inert and
  silently costs the whole dynamic report (reproduced on the corpus Enhanced FLV: its mastering
  display and MaxCLL vanished). **The `vpcC` arm names its codecs exactly rather than matching
  `Other(_)`**, because `parse_vpcc_record`'s only structural test is a leading `0x01` — which is
  `configurationVersion` in an `avcC` and an `hvcC` too, so a wildcard reads those records'
  constraint bytes as VP9 fields and fabricates a full CICP description tagged `Container`, i.e.
  an HDR10 verdict invented from an unrelated record. And **`videodatarate` is range-checked
  after it is scaled, not before**: a declared rate above `f64::MAX / 1024` multiplies to
  infinity, which serialises as JSON `null` and breaks the schema's "always a float" guarantee for
  `bits_per_sec` — the same product-versus-factor shape as the AVI index defect, a third time.
- `container/ogg.rs` — Ogg (`.ogv`, `.ogg`, `.oga`, `.ogm`, `.ogx`), read against **RFC 3533**,
  carrying Theora ([`theora.rs`]) and VP8. A flat chain of pages each declaring its own length in a
  segment table, so the walk is arithmetic on declared sizes, and everything the report states sits
  in one packet alone on its stream's first page — a bounded head read plus, for the duration
  alone, a bounded tail read. `chunks` stays **empty by design**: neither codec has a bitstream
  side channel this project reads, so there is nothing to sample (the `mpegv`/`asf` contract).
  Seven facts are invariants. **Endianness flips at the page/payload seam** — page headers are
  little-endian (§6, "LSB first") while both codec mappings are big-endian and Vorbis in the same
  file is LSb-first bit-packed, so one `.ogv` carries three conventions. **A granule position of
  -1 means no packet finishes on that page**, ordinary mid-stream for a packet spanning pages, and
  read as a count it makes the last page an enormous frame total. **The physically last page need
  not be the video's** — a Theora+Vorbis mux routinely ends on audio, so the tail is scanned for
  the last page *of the video serial*. **A chained file's tail describes only its final link**
  (Ogg permits concatenation with fresh serials, and it is Theora's documented way to change frame
  rate mid-file), so any serial the head's BOS run did not declare, or a BOS flag past that run,
  yields `None` — hdrprobe is the only one of the three tools that neither invents a duration here
  nor a bitrate from it. **The two windows must stay disjoint, and "file smaller than the window"
  is only half of it**: a file a *little larger* starts its tail scan inside its own BOS run and
  reads those pages as a second link, so the start is clamped past `bos_end` — the same
  `tail_start.max(head_end)` the program-stream backend carries, and a band as wide as the last BOS
  page's offset. **A tail anchor is believed only when its run tiles the rest of the file**: two
  27-byte pages satisfy any fixed-depth chain check, so 54 bytes planted at the scan's start
  position redirect it and their declared granule becomes the duration; `chain_holds` is therefore
  only a cheap filter ahead of
  `tiles_to_end`, whose page-parse budget is shared across candidates so their product cannot grow.
  And **the header packets are not video payload** — a comment header may carry cover art — so
  `--full`'s exact byte sum skips each stream's first packets by counting lacing values rather than
  by assuming where pages divide. Two more traps in the `--full` walk: **a broken chain is not
  truncation**, and the stream-structure-version byte is the one page-header field whose failure
  `parse_page` reports identically to a short final page — read as truncation it returns a short sum
  that main.rs divides by the whole file's duration, measured at 805 kb/s against a true 2.31 Mb/s
  (a 1.7 MiB Theora file with one mid-file page's version byte flipped); and
  the walk plan is set **only for a single-video file**, because `sample::scan` returns one
  `TrackScan` per raw-stream walk and main.rs zips it against the tracks, so a plan on a two-video
  file would drop the second entirely. `track_number` is the logical bitstream's **serial number**,
  a random 32-bit value rather than a small ordinal. `duration_secs` is the **video** stream's, not
  the longest stream's: for every ordinary file they agree, but a mux whose audio far outlasts its
  video reports the video's length and an `overall` rate computed against it (3 s and 630 kb/s on a
  fixture holding 3 s of video and 90 s of audio, where ffprobe says 90 s) — knowing the true
  length means decoding audio granule positions, which is out of scope plan-wide, and risk 15 in
  `dev/sdr-coverage-plan.md` records the alternatives and why each was declined. Only `.ogv` is in
  `main::VIDEO_EXTS`: `.ogg`
  and `.oga` are overwhelmingly audio (the `.wma` reasoning), and `.ogm`/`.ogx` are left out because
  OGM video has no parser here, so a scan of them could only ever print one error per file.
- `container/ps.rs` — MPEG program stream / MPEG-1 system stream (`.mpg`, `.mpeg`, `.vob`,
  `.m2p`, `.evo`), read against **ITU-T H.222.0 (10/2014) | ISO/IEC 13818-1**, which is free
  from ITU and is the normative source for the pack header (Table 2-39), the PES packet
  (Table 2-21) and the stream_id assignments (Table 2-22); the MPEG-1 pack and PES layouts are
  11172-1's, are *not* in H.222.0, and come from ffmpeg plus corpus validation. Structurally the
  TS backend's twin — a bounded head window reassembles the video elementary stream out of
  scattered PES payloads, a bounded tail window closes the timeline — so the pieces both need
  live in `mod.rs`. Five facts are invariants. **The stream id names a stream number, not a
  codec**: Table 2-22's `1110 xxxx` row covers H.262, 11172-2, 14496-2, H.264 *and* H.265, so
  ffmpeg's habit of writing H.264 at `0xE2` is a muxer convention and routing on it would be
  wrong. **The reassembled ES is routed by a whole-head start-code census, never by its first
  byte** (`classify_es`): a video PES payload often opens on a picture start code, `00 00 01 00`,
  whose `0x00` passes `looks_like_nal_header`'s HEVC reading about seven times in eight, so
  first-byte routing has no verdict to give at all on 96% of mid-file cuts (measured: the
  reassembled stream opens mid-picture, with no start code). The census is sound
  rather than probabilistic — emulation prevention means a conforming H.264/H.265 stream cannot
  contain `00 00 01` followed by a byte `>= 0x80`, so one such code refutes Annex-B outright —
  and the MPEG readings are tried first because the reverse order lets a DVD's tens of thousands
  of slice start codes eventually yield a byte run that decodes as a valid SPS (the documented
  way a VOB once reported a full HEVC profile). **The decisive Part 2 codes are `B0`/`B1`/`B6`
  and must never include the VOL range `20`..`2F`**: that range is also MPEG-2's
  `slice_start_code` space, where the value is `slice_vertical_position`, so slice `0x20` is
  macroblock row 32 — every format above standard definition emits it, and it is *also* where
  HEVC's IRAP NAL headers (`26`/`28`/`2A`) and H.264's `nal_ref_idc == 1` slices (`21`) land.
  Including it misread PAL and HD MPEG-2, HEVC-in-PS and H.264-in-PS alike, the last of those
  fabricating a complete `1639x6058 · 0.692 fps · BT.709 limited` report out of slice bytes.
  Nothing is lost by excluding it: `B6` (VOP) rides every Part 2 frame. **`PES_packet_length` is
  the only legal way to
  advance**: a byte scan that resumes inside a packet reads audio and private payload as
  structure, which on the corpus DVD invents seven Program Stream Maps on a disc that has none.
  **An access unit is cut at the payload's first start code, never at its first byte**: §2.4.3.7
  ties a timestamp to the unit whose first picture start code *commences in* that packet, which
  need not be at offset 0, and the flag that would guarantee alignment
  (`data_alignment_indicator`) is clear on every file observed — on ffmpeg-written program
  streams only 1 to 3 of 42 to 50 timestamped packets open on a start code. Cutting at the first
  byte hands `split_annexb` a chunk starting mid-slice, and since it reads a chunk's offset 0 as
  an implicit NAL boundary it mints a NAL from compressed payload: reproduced growing a
  *signalled* 396272 cd/m² mastering display and MaxCLL 44200 on a stream carrying neither.
  **The SCR is not a duration** — see the duration invariant below. And **the pack variant names
  the container, never the codec**: `ffmpeg -f mpeg` writes an 11172-1 system stream carrying
  MPEG-2 video, so the corpus `mpeg2.mpg` is correctly `MPEG-1 System Stream` + `MPEG-2 Video`.
  The head census (`looks_like_program_stream`) is a faithful transcription of ffmpeg's
  `mpegps_probe`, thresholds and payload-skip arithmetic included, on the reasoning that those
  thresholds encode two decades of misidentification reports; it is also the backend's own head
  gate, which a backend reachable by extension needs. There is deliberately **no program stream
  map parser**: it is absent from DVDs, from consumer `.mpg` and from ffmpeg's own muxer, the
  content routing above covers every case it would answer, and a mis-parsed one could only
  override a correct verdict.
- `dv/` — `rpu.rs` (libdovi wrapper + panic guard), `levels.rs` (title-stable aggregation),
  `ccid.rs` (the Dolby "Profiles and Levels" tables as data: profile -> admitted CCID(s),
  CCID -> the five-part base-layer VUI as **CICP code points**, the reverse lookup
  `infer_ccid`, the `hdr10_base` gate, the CCID label set, and the withdrawn 8.3/8.5
  pairings). **Every CCID/profile/VUI fact resolves here** — the change that created it
  existed to delete six hand-rolled projections of these tables, so a seventh is a
  regression, not a shortcut. (Profile 8's `8.1` convention default is *not* one of those
  facts: it is ecosystem practice rather than a spec-table row, which is why it legitimately
  lives in `levels.rs` — in `resolve_compat` and `dv_profile_label` — and nowhere in the
  tables.) Rows hold codes, never display labels, and names come from
  the shared `container::cicp_*` decoders so a derived label can't drift from a signalled
  one. The admitted-CCID sets are the **union across spec revisions** (v1.5 narrowed P8 to
  {1,4} and P10 to {0,1,4}; 8.2 content is everywhere) — narrowing would refuse to name
  legal streams. Sources and page cites are in the module docs; every row is pinned by a
  test naming its table.
- `hdr/` — `mod.rs` (format classification + `primaries_label`, the chromaticity→gamut matcher
  behind the Mastering line's tag), `sei.rs` (ST.2086/CLL/HDR10+/SL-HDR/HDR Vivid/
  alt-transfer; the T.35 dynamic formats are told apart by country + provider code —
  0xB5/0x003C HDR10+, 0xB5/0x003A ETSI SL-HDR (mode digit 1/2/3 names the variant, e.g.
  "SL-HDR2 / HDR10"), 0x26/0x0004 CUVA HDR Vivid (2-byte oriented code = version, e.g.
  "HDR Vivid / HLG"; the AV1 T.35 OBU route in `av1/obu.rs` shares the parser). HDR Vivid
  also has a container declaration, the MP4 `cuvv` sample-entry box (`mp4.rs::parse_cuvv` →
  `TrackDemux::cuvv_version_map`, zero extra I/O — the box rides the already-parsed stsd):
  either signal is presence, the box's bitmap wins the reported version, and box-only
  detection keeps `--no-rpu` honest (no frame reads)). The AV1
  `HDR_MDCV` OBU shares ST.2086's 24-byte shape but **not its semantics** — R/G/B (not G/B/R)
  primary order, 0.16 fixed-point chromaticities, 24.8/18.14 fixed-point luminance — so it has
  its own `sei::parse_mastering_av1`; routing it through `parse_mastering` mis-scales max
  luminance by ~39× (10000 nits read as 256).
- `sidecar/` — metadata-only inputs that bypass the video pipeline: `rpu_bin.rs` (raw DV RPU
  `.bin`/`.rpu`), `dv_xml.rs` (DV CM XML), `hdr10plus_json.rs` (hdr10plus_tool JSON); `mod.rs`
  detects by extension and renders through the ordinary `Report`. DV sidecars carry no
  resolution, so L5 is sized against an assumed UHD canvas (`ASSUMED_CANVAS`) and footnoted.
  A DV XML's Level-0 globals **frame rate** and **mastering display**, and its **schema version**,
  are read straight from the raw XML in `dv_xml.rs` (`<EditRate>`, `<MasteringDisplay>`
  peak/min/primaries, and the root `version` attribute / `<Version>` child — the same pair libdovi
  accepts), *not* from libdovi: `CmXmlParser` never parses `<EditRate>`, folds the mastering
  display into a lossy PQ code, and reduces the version to a coarse enum, so reading the XML gives
  exact values. All sit in the file head, so it's cheap; keep them off the libdovi path. The
  version renders as the General section's `Schema version` line (`model::Report::format_version`),
  present only when an input declares one — today only DV XML sidecars. A sidecar's `Report`
  carries one `video_tracks` entry (empty `codec`) so JSON consumers iterate the array for
  every input kind. The XML's Level-0 primaries (tagged `[L0]`) are the
  mastering-gamut fallback for a CM v2.9 XML, which has no L9; a recognized L9 wins when present,
  so CM v4.0 output is unchanged.
- `bdiso/` — Blu-ray ISO (`.iso`) main-feature probing: `udf.rs` (read-only ECMA-167/UDF 2.50
  walker over the ISO mmap, both plain type-1 partition maps and the 2.50 Metadata Partition,
  bounds-checked with `mp4.rs` discipline; UDF is little-endian, explicit LE reads), `mpls.rs`
  (playlist header + PlayItems only, big-endian; STN tables and angle blocks are skipped by the
  item length field), `mod.rs` (`is_udf_iso` VRS gate, `locate_main_feature`, the `select_main`
  heuristic: longest deduped-segment duration wins, ties by referenced clip bytes, identical
  playlists collapse, missing-clip playlists drop; probe clip = the winner's largest clip,
  extents coalesced to one contiguous range). `main.rs` owns the orchestration: the extension
  gate, the clip subslice, the based Frontier, the `"Blu-ray ISO (BDMV)"` container label, and
  `model::BdIso` (the `Main feature` line). The synthetic UDF image builder for tests lives in
  `udf.rs::testimg` (in-memory, path-portable; type-1 images use short_ad file data and
  extent-recorded directories, metadata images use long_ad data and inline directories, so
  both descriptor forms stay exercised).
- `sample.rs` (parallel sampling), `model.rs` (serde report tree), `render.rs`, `bits.rs`.
  The JSON output is an external contract documented field-by-field in `docs/SCHEMA.md` and
  versioned by `model::SCHEMA_VERSION` (the `hdrprobe_schema_version` field on every report,
  independent of the crate version — named to distinguish it from the input's own
  `format_version` and Dolby's `cm_version`): any change to `model.rs` (fields, presence
  conditions) or to a rendered label value space (container/codec/profile/format strings,
  enumerated names) must update the document and bump the version — minor for additive
  (new field, new enumerated value), major for breaking (rename/removal, type/unit/presence/
  meaning change). The golden shape test in `model.rs` pins the serialized field paths, so a
  model change fails `cargo test` until the expected list, version, and document move together.
- `prefetch.rs` — warms the byte ranges the parse is about to fault, in two stages: container
  metadata before demux (`warm_metadata`) and the selected sample AUs after demux
  (`warm_sample_chunks`), both executed by `warm_ranges` (sort, coalesce, then concurrent
  pipelined positioned reads), so SMB/NFS scans don't fault them in over hundreds of
  round-trips. Timing only: parsing still runs against the mmap. Gated to remote volumes by
  `is_remote`, decided from the open handle (Windows `FileRemoteProtocolInfo`), which costs no
  extra network round-trip and is correct through mapped drives, UNC, symlinks, and subst.
- `progress.rs` — `--full` progress reporting (`--progress auto|bar|json|off`): a stderr bar in
  the active theme's palette — a `Scanning: <name>` header line once per file (it carries the
  file name and the `[k/N]` counter), then **one** unlabeled `\r`-rewritten bar line for the
  whole file beneath it (two-tone fill: solid bright cells plus a mid-tone `▓` half-cell at the
  leading edge; the terminal cursor is hidden while the bar rewrites and restored on every exit
  path, including the error `Drop`), no matter how many internal phases run: `bar_fraction` blends
  an `Index` walk into the bar's first half and the scan that follows into the second (a lone
  scan owns the whole bar — the common case), so the percent is monotonic by construction and
  can never reset mid-file (a bar restarting at 0% reads as a loop/hang, a real user report —
  never reintroduce a per-phase reset; the *JSON* events stay per-phase, that contract is
  unchanged); on the decorated interactive path each *successful* file's whole progress display
  (header, spacer, bar) is erased in place when it finishes (`Progress::finish_erased`: a
  cursor-up over the header's wrapped-row count recorded at print time, then ED0) so the file's
  streamed report prints where the header stood — the screen accumulates clean reports with the
  live bar always at the bottom, and there is **no end-of-run screen clear** (one would wipe
  reports the user is already reading; the old ED2/ED3 clear is gone, which also made the shell
  verb's hidden `--own-console` flag inert — it stays accepted and stays in the verb command
  strings because user registries persist them across upgrades) —
  or NDJSON events on stderr (contract documented in
  `docs/SCHEMA.md`, "Progress events"; the event structs live here, *not* in `model.rs`, so the
  report schema and its golden shape test are untouched). One `Progress` per file, created in
  `main` and threaded through `container::demux` and `sample::scan`; two byte-denominated
  phases, `Index` (a demux-time walk past the head window — only the rare metadata rescues:
  TS `sps_rescue`, raw-HEVC `rescue_sps`, raw OBU's no-sequence-header fallback) and `Scan` (the sampler:
  per-batch in `scan_chunks`; by walk position on the TS, MKV, and raw fused streaming paths —
  all single-phase, so a normal `--full` run of any container is one `Scan` from 0 to 100).

## Invariants that are easy to violate

- **Zero-copy mmap `Chunk` model.** A `Chunk { offset, size }` is a byte range into the mmap;
  payloads are never copied up front. A backend may leave `chunks` **empty**: `sample::scan`
  returns early when every track's list is, so a metadata-only backend needs no chunk index and no
  sampler arm on the default path, though a `--full` walk that must read payload still needs its
  own streaming plan on `Demux` (the `ts_stream`/`mkv_stream`/`raw_stream` shape below). The full
  contract is on the field's own doc comment. **Every container backend is hand-rolled on purpose** —
  do *not* add `matroska-demuxer`/`mp4`/etc.; they copy frame data and hide byte offsets, which
  breaks this model. The **one exception is TS/M2TS**, which scatters the elementary stream
  across packets: it fills `Demux::reassembled: Option<Vec<u8>>` (the bounded head window only)
  and `chunks` index into *that*. `sample.rs` picks the source via
  `reassembled.as_deref().unwrap_or(mmap)`. All other backends leave it `None`. Under `--full`
  the whole video ES is **never materialized**: demux exposes `Demux::ts_stream`
  (`ts::TsFullStream`) and `sample::scan` drives the resumable `ts::EsStreamer` through the file
  in `ts::STREAM_WINDOW_BYTES` windows, reusing one scratch buffer — so a `--full` scan of a huge
  M2TS holds ~150 MB of heap, not the whole video track (measured: 1.4 GB M2TS, 1.87 GB → 155 MB
  peak private commit; the old path scaled with file size, an OOM on a 60 GB remux). Partial AUs
  carry across windows inside the streamer; the trailing AU still accumulating at EOF is never
  flushed (no terminating PES start bounds it), matching the historical one-shot pass and the
  bitrate byte count. Don't reintroduce a whole-stream buffer, and don't flush that trailing AU.
  The sampler itself is memory-bounded the same way for **every** container: `sample::scan_chunks`
  extracts in `AGG_BATCH` parallel batches and aggregates each batch sequentially in index order,
  so `--full` never holds every frame's parsed RPU at once. That order is load-bearing
  (`DvAggregate` has first-wins fields and its L5 insertion order is the rendered order;
  `SeiFindings::merge` is first-wins) — **never replace the batch loop with a parallel reduce of
  partial aggregates**. **Fragmented MP4 (fMP4/CMAF) stays zero-copy too**: its moov `stbl` tables are
  present but *empty* (a silently empty report, not an error), so when they yield no samples and
  `moof` boxes exist, `mp4.rs` builds the index from each fragment's `tfhd`/`trun` tables instead
  (`build_fragment_index`) — sizes/durations fall back tfhd → `mvex` trex defaults, every traf is
  walked (not just the video track's) because a traf without an explicit base offset chains off
  the end of the previous traf's data, and an unsizable run is dropped, never guessed. The summed
  trun sample durations are the track's own exact duration: they feed fps and the bitrate
  denominator (`stream_duration_secs`, matching MediaInfo's video-track rate), while the Duration
  line keeps the mvhd presentation value, falling back to the sum and then `mehd`.
- **Third-party parsers can panic, not just `Err`.** libdovi and the `hdr10plus` crate abort on
  some malformed input. Route *every* call into them through `dv::rpu::guard` (`catch_unwind`).
  **Never re-add `panic = "abort"` to the release profile** — it turns the guard into a no-op.
- **Report title-stable DV levels only.** Show profile/level/compat, L254 (CM version), L6, L9,
  L11, and the *set* of L2/L8 trim targets. Never emit L1 or per-shot trim *values*.
  **MaxCLL/MaxFALL is HDR10 (CTA-861.3) signaling whose only consumer is an HDR10 base**
  (compat id 1, or 6 for UHD Blu-ray) — one gate, `ccid::hdr10_base`, shared by
  `hdr::assemble`'s mastering/CLL fallbacks and the text report's own L6 line, which used to
  carry a second copy of the rule that could drift. Every other base still carries L6 on every frame but it
  is inert there: on IPT-PQ-C2 (compat 0: P5/P20/AV1 10.0) and HLG (compat 4: 8.4/10.4) the CLL
  half is a zeroed placeholder (corpus-verified, including Dolby's own P5 demo and the 8.4/10.4
  samples), and an SDR base signals no static metadata either (the P9 corpus file's *filled* L6
  is not counter-evidence: it is a frankenstein built from a real HDR title's RPU, not Dolby P9
  tooling output). So unless the base is HDR10 the text report drops the L6 line (`render.rs`;
  **with no *resolvable* id the profile major decides: P7/P8 default to HDR10, P4/P5 do not** —
  after the spec and inferred rungs that state is reachable only for a P8 admitting CCID 1, 2
  and 4 with nothing to separate them, and the heuristic must stay explicit: it gates the
  corpus-verified L6/MaxCLL suppression, so losing it would be an invisible regression) and the HDR
  section's CLL *and* Mastering lines never fall back to L6 (`hdr::assemble`, both gated on
  `hdr10_base`; the L6 mastering half is just the grade's display, already on the DV Mastering
  line); a *signalled* MDCV/CLL box or SEI still shows, and the JSON keeps `dolby_vision.l6`
  verbatim. **L5 is the
  deliberate exception**: it varies with aspect changes, so it's sampled and shown as the set of
  distinct active areas, marked with the sampled footnote (a `*` on the row label, explained once
  at the report's foot; a `--full` scan carries no mark — absence reads as complete). The
  **trim-target set carries the same sampled footnote**: the L8 half is per-shot in real titles
  (corpus-verified: a BD original whose head shots carry only the 100-nit L8 while other scenes
  add 600), so a sampled union may be incomplete. An L8 trim's
  `target_display_index` maps to nits via `levels::resolve_l8_nits`: a **custom index (255, common
  in Profile 20) is defined by an L10 block in the same title**, so it's resolved from the title's
  L10 target-display map (`target_max_pq` -> nits) before the predefined index table; unknown with
  neither is dropped, never guessed (the `hdrprobe` table is preferred over libdovi's
  `trim_target_nits()`, which guesses 100 for 255). The **provenance tag is per-value and dynamic** —
  each target carries its own `levels` (`model::TrimTarget`), so a single value renders `600 [L2]`,
  a value produced by both levels `100 [L2/L8]`, and an L8-only title like Profile 20 `300 [L8]`.
  **L10 is never in the tag, but an L10-defined display counts as an L8 target**
  (`levels::merge_trim_targets`): L2 is self-contained (it carries its target's nits directly),
  so a display index is a CM v4.0/L8 mechanism by construction — an L10 definition can serve
  nothing else, making the defined display a *custom L8 target* even when no read L8 referenced
  it. Unlike the per-shot trims, the definition rides every RPU's global extension payload (it
  is the compiled form of the CM XML's Level-0 target-display list — the displays trims were
  authored for), so it is title-level evidence independent of sampling; presets never get L10,
  so this recovers only custom targets. Folded into the L8 set, rendered `[L8]` — `[L10]` would
  leak bitstream plumbing into a report whose readers know the L2/L8 trim levels.
  The **metadata cadence** verdict (`model::MetadataCadence`, the `Metadata cadence` line) is the
  one title-stable fact reported *about* the omitted per-shot levels: whether they were authored
  shot-by-shot or frame-by-frame, decided by comparing consecutive frames' DM payloads
  (`levels::dm_fingerprint` — extension blocks serialized to their bitstream form plus
  `source_min/max_pq`; `scene_refresh_flag`, the composer/NLQ payload, and **L4** are excluded on
  purpose: the flag so a shot's first frame compares equal to its shot, the composer so a FEL's
  per-frame mapping can't masquerade as CM changes, and L4 because its temporal-filtering anchors
  are a per-frame running average *by mechanism* even in shot-based authoring — corpus-verified
  via dovi_tool export on the P7 FEL CM v2.9 clip, where adjacent same-shot frames differ only in
  L4 while the clip's own CM XML is per-shot; with L4 in the fingerprint most of the corpus
  misread as per-frame). Pairs count only when folds are *every* frame in stream order
  (`DvAggregate::track_consecutive`: the `--full` scan and the RPU-bin sidecar's run collapse) or
  come from the DV XML's declared shot/edit structure (`add_cadence_pairs`); the sampled default
  never gets a verdict — a pair spanning a sampling gap would read as a change, so don't widen
  the gate. The per-frame line is a *quarter* of pairs changed, not a majority: shot-based
  changes are 1/(avg shot length), corpus-observed at 0–2.6% including decode-order stragglers at
  open-GOP cuts, while per-frame titles observe 55–64% (static stretches produce equal
  neighbours) and would halve again on a duplicated-frame high-rate stream — still above a
  quarter. The RPU-bin and DV-XML paths cross-validate on the corpus feature (identical
  pair/change counts from independent computations), and the P7 FEL MKV matches its own CM XML
  exactly (6/713).
- **DV facts and their sources.** BL **compatibility id** and DV **level** come from the
  `dvcC`/`dvvC` box, *not* the RPU. When **no config declares a level** (authentic disc M2TS —
  UHD-BD signals DV via the playlist, not the PMT — or a raw elementary stream), it is derived
  from the coded stream's resolution and reported frame rate against the Dolby P&L table
  (`levels::fill_derived_level`, a main.rs-only post-pass like the Mastering badges; JSON-only
  via `dolby_vision.level_derived`, never text-rendered): the smallest level admitting the pixel
  rate and width, a pixel-rate floor only (the bitrate/tier axis is not probed — the level ID has
  no tier field, so a bitrate cap can only ever say a stream is non-conformant *at* its level, and
  feeding it to the selector would push a high-bitrate 1080p24 up to level 6; the axis is also
  unavailable exactly where the derivation runs, since raw ES has no bitrate at all and TS has
  only an `overall` one that counts audio). **`DV_LEVEL_LIMITS`' two columns come from two
  different spec columns and disagree**: the rate is the row's anchor format (`w × h × fps`), but
  the width is the row's own *Maximum decoded bitstream video width*, which is deliberately wider
  than the anchor on levels 4 and 5 (2560 and 3840 against a 1920-wide anchor). Deriving the width
  from the anchor instead reads plausible and silently pushes ultrawide-but-low-rate content up
  two levels — take each column from the spec, never one from the other. A declared
  level always wins, sidecars never derive (assumed canvas), and no fps means no level — never
  a guess. The DV Mastering line's **luminance** is the DM header's
  `source_min_pq`/`source_max_pq` (present in every CM version); its **gamut** comes only from a
  level that actually carries one — RPU L9 (CM v4.0) or a DV XML's Level-0 `<MasteringDisplay>` —
  tagged `[L9]`/`[L0]` per `model::MasteringDisplay::primaries_level`. A CM v2.9 RPU carries **no
  mastering primaries at all**: the DM header's `ycc_to_rgb`/`rgb_to_lms` matrices are the
  *signal* space, not the display — corpus-verified: P3-D65-mastered titles (v2.9 per their BL
  MDCV, v4.0 per their own L9) all carry the identical BT.2020 `rgb_to_lms` (see the comment in
  `levels::finalize`) — so never fingerprint them into a gamut name; the v2.9 line stays
  luminance-only. Everything dynamic (FEL/MEL, L5/L6/L9/L11/L254, trim
  targets) comes from the **RPU**, which rides the base layer / a DV-flagged track — the
  enhancement-layer *residual* is decode-only and never needed. This is why P7 dual-track
  "just works" once the BL/DV track's RPU is parsed. The **compatibility id is `Option<u8>`**:
  the older/compact 4-byte DV record (Profile-4 TS `0xB0` descriptors) omits the compat nibble,
  so `parse_dovi_config` requires only 4 bytes and reads compat when present, else `None` — never
  a guessed 0. The **TS `0xB0` descriptor is not byte-identical to the ISOBMFF `dvcC`**: per
  Table 3-2 of the Dolby "MPEG-2 TS Format" spec it inserts a `dependency_pid`(13)+reserved(3)
  block before the compat nibble **when `bl_present_flag == 0`** (the secondary EL/RPU PID of a
  dual-PID stream, e.g. P7 dual-track M2TS). So the TS path parses through
  `parse_dovi_ts_descriptor` (which skips that block), *not* `parse_dovi_config` — routing a TS
  descriptor through the ISOBMFF parser reads the compat nibble 16 bits early (P7 dual-PID showed
  a bogus `8` instead of `6`). The compat id becomes the profile's minor digit
  (`levels::dv_profile_label`: `7.6`, `8.1`, `10.4`, …). **Profile 4 is dual-layer** (like P7): its EL presence and MEL/FEL tag come from
  the config + RPU the same way, and its **SDR base comes from the spec rung**
  (`ccid::spec_ccid(4)` is 2, P4 being SDR-compatible by definition), which is what covers old
  P4 muxes carrying neither a compat id nor a base-layer transfer VUI — `hdr::assemble` itself
  only reads the resolved id. The **reconstructed bit depth**
  (`model::DolbyVision::reconstructed_bit_depth`, the report's `Reconstruction` line) is the RPU
  header's signaled `vdr_bit_depth` read verbatim — **never assumed from the profile**: P7 FEL
  signals 12 but P4 FEL signals 14 (corpus-verified on every frame, in both the header and the DM
  header's independent `signal_bit_depth`; libdovi's own P7-vs-P4 detector keys on
  `vdr_bit_depth == 12`, and its P4 template carries `signal_bit_depth: 14`). The semantics and
  name come from the field's public definition — ETSI GS CCM 001 v1.1.1 §"hdr_bit_depth_minus8":
  "used to derive the bit depth of **the reconstructed HDR signal**" — with one caveat for
  context: the ETSI-standardized subset allows only 10/12, so P4's 14 is Dolby-proprietary
  signaling that predates the 2017 ETSI publication (which is why libdovi's validator accepts
  `vdr_bit_depth_minus8 <= 6`, not ETSI's 4, and why Dolby's Profiles & Levels spec needs a
  translation table to map DV profiles onto ETSI CCM profiles). It is **FEL-gated**
  in `levels::finalize`: every RPU signals a vdr depth (MEL and single-layer titles all say 12,
  corpus-verified), but only a FEL residual carries real data to reconstruct beyond the base
  layer — elsewhere the value is composer arithmetic precision, so rendering it would misread as
  content depth. The gate itself is Dolby-published, not community convention: "Dolby Vision
  Profiles and Levels" v1.3.2 Annex II's MEL fingerprint (zero `nlq_offset`/`vdr_in_max`/
  deadzone, `vdr_in_max_int` 1) is exactly what libdovi's `is_mel()` checks, so `el_type` FEL/MEL
  follows Dolby's own detection method. The "10-bit BL" half of the rendered line is safe because
  libdovi's header validation rejects any RPU whose BL/EL depths aren't exactly 10-bit (matching
  ETSI CCM's `BL/EL_bit_depth_minus8` constraint and the 10-bit HEVC BL/EL codec in the P&L
  spec's profile tables).
- **FEL brightness expansion is a metadata verdict with hard gates.** The DV Mastering line's
  `(FEL brightness expansion)` badge (`levels::flag_fel_brightness_expansion`) fires only when
  the RPU is **FEL** *and* the grade's `source_max_pq` exceeds the **base layer's own** declared
  mastering max (container MDCV / ST.2086 SEI) by >10% (e.g. 4000-nit grade over a 1000-nit BL).
  Never flag a MEL (its residual is empty, so it can't out-bright the BL no matter what the
  displays say), never compare against the RPU's own L6 values (self-referential), and never flag
  sidecars (no base layer to expand beyond), so `main.rs` is the only caller. This is a metadata
  verdict only: confirming the general case would mean decoding and comparing composed-vs-BL
  pixels, which hdrprobe never does, so a missing badge is not proof of no expansion. Its
  sibling verdict is the **`MDP mismatch` badge** (`levels::flag_mastering_primaries_mismatch`,
  same main.rs-only call site, rendered on the same DV Mastering line, JSON
  `dolby_vision.mastering_primaries_mismatch`): the grade's recognized **L9** gamut name vs the
  base layer's **signalled** MDCV box / ST.2086 SEI label, compared by plain string equality
  (both sides come from the one `hdr::primaries_label` value space, which already absorbed
  quantization — never re-compare coordinates with a second tolerance). Hard gates: L9
  provenance only (`primaries_level == 9`, so a DV XML's L0 never fires), a signalled BL label
  only (never the L6 fallback, whose primaries *are* the L9 — self-comparison), both sides
  recognized (unmatched coordinates suppress the verdict, never guess). **Both sides are a mastering
  display, but from different pipeline stages, so a difference does not make either one wrong.**
  Dolby's "Dolby Vision Metadata Levels" defines L9 as "the color primaries and white point of the
  Mastering Monitor/Display used for the project", calculated from the display "selection made by
  the colorist *during the Dolby Vision content creation process*" — the DV pass, a step after the
  base layer's own grade and often in a different suite with a different colorist working from a
  delivered master. The base layer's ST.2086 describes *its* grade's display. Two accurate values
  can therefore disagree, which is exactly why no spec requires them to match. (L0's Mastering
  Display is the same DV-pass selection as L9, so the `[L9]`/`[L0]` tags on the DV Mastering line
  are not a conflation.) Do not re-derive L9's meaning from observed
  values: `<SourceColorPrimary>`, libdovi's per-shot `parse_level9_trim`, and the studio spec's
  separate "Colour Encoding Primaries" row all *suggest* an encoding quantity, and that reading is
  wrong — the encoding primaries are the VUI/container ones, and the RPU carries no such field.
  **A difference is a quirk, not a spec violation**: the two values come from different
  specifications written by different tools (L9 by the DV authoring tool, ST.2086 by the encoder or
  muxer), nothing requires them to match, and Dolby's studio spec pointedly attaches "must match
  corresponding video content" to colour *encoding* primaries while attaching no matching rule at
  all to the mastering display primaries. So the badge is a provenance observation, not an error
  claim — and **both directions occur**: the corpus's only firings are a P3-D65 base layer against
  a BT.2020 L9 (the reverse of re-encode drift), in the `dv7fel_dt` frankenfile *and* in Dolby's
  own OTT reference streams, so never re-document it as one-directional. The third sibling is the **`Unconverted RPU` chip**
  (`levels::finalize`, rendered on the Profile line, JSON
  `dolby_vision.unconverted_dual_layer_rpu`): the RPU carries the dual-layer NLQ composer
  payload (`el_type` is Some — that fingerprint exists only in P4/P7-authored RPUs) while the
  carriage demonstrably has no EL. Hard gates on the no-EL side: an explicit dvcC/dvvC/
  descriptor with `el_present == 0`, or AV1 with no config (DV-on-AV1 is single-layer by
  construction — the same derivation `finalize` already uses for `el`). Never fire it for
  config-less HEVC (a raw P7 BL+EL Annex-B stream carries its EL in-band) or metadata sidecars
  (no carriage to compare; they reach `finalize` with `cfg == None`). The classic producer is a
  custom transcode that injected a UHD-BD P7 RPU without dovi_tool `--mode 2` — also the root
  cause of out-of-spec AV1 profile digits like `10.6`, since mkvmerge hardcodes AV1's profile
  to 10 but derives the dvvC compat id from the RPU-guessed profile (6 exactly when the guess
  is 7). The stray payload is inert for playback (a MEL residual contributes nothing), so this
  too is a provenance observation, not an error claim.
- **A Blu-ray ISO is probed as a clip subslice, never as offset fix-ups.** The ISO path
  (`main.rs`, gated on the `.iso` extension *and* `bdiso::is_udf_iso`) resolves the main
  feature to one contiguous byte range and hands `&mmap[clip_start..clip_start+clip_len]` to
  `ts::demux` **and** `sample::scan`: the TS backend is fully slice-relative (packet phase
  re-derived, head/tail windows addressed from the slice ends, chunks index the reassembled
  heap buffer), so bitrate denominators, `--full` streaming positions, and progress totals are
  all clip-correct by construction. Never pass the whole ISO mmap with offsets patched in.
  The `--full` frontier is the one base-aware piece: `Frontier::new_at(file, clip_start,
  clip_len)` keeps walk positions slice-relative and translates only the reads. Related
  gates, all deliberate: the **AACS verdict** is "`ts::detect_layout` fails on the clip head
  *and* an `AACS/` directory exists" (decrypted backups keep the directory, so presence alone
  never rejects; encrypted clips can't sync-lock because AACS leaves only 16 clear bytes per
  6144-byte unit); **selection dedupes segments before comparing durations** (a decoy looping
  one segment 500 times collapses to one, so it can't out-rank the feature); a **fragmented
  clip errors honestly** (UDF's ~1 GiB extent cap makes real features many exactly-adjacent
  extents that coalesce; a genuine gap is not supported, never guessed). Prefetch:
  `looks_like_iso` is extension-only on purpose (a content sniff would fault the sector-16..64
  VRS window on every remote non-ISO file), `ISO_HEAD_WARM` (1 MiB) covers VRS + front VDS +
  the anchor at byte 512 KiB, the locator warms the metadata-partition and playlist extents
  exactly (`warm: Option<&File>`, remote only), and `prefetch::warm_ts_windows` replays the TS
  head/tail warm at `clip_start`/clip EOF (keep it in sync with `ts::HEAD_SCAN_BYTES`/
  `TAIL_SCAN_BYTES` like the byte-0 TS branch). The report keeps the ISO's `size_bytes` and
  the clip's PCR `duration_secs`; the playlist's own edit duration renders on the
  `Main feature` line, never on the Duration line.
- **Adding a backend means adding its extensions to `main::VIDEO_EXTS`, and that is outward-facing.**
  `container::demux`'s extension map decides what a *named* file dispatches to; `VIDEO_EXTS`
  decides what a **directory scan** picks up, and they are separate lists that have silently
  disagreed before — Phase 1 shipped `.m2v`/`.m1v`/`.mpv` support that worked when a file was
  named and was invisible to `hdrprobe rips/`, which is precisely the goal statement's failure
  mode. `shell.rs` builds the Windows context-menu verb's file-type list from the same constant
  (plus `SIDECAR_EXTS`), so every addition also registers those extensions in the user's registry
  on the next `--install-shell`; that is intended, and worth naming in a commit message rather
  than discovering later. Two deliberate omissions, both because the extension is claimed by
  something a directory of mixed files is full of: `.bin`, which the raw-HEVC dispatch accepts,
  and **`.wma`**, which the ASF backend accepts because a `.wma` may legitimately carry video —
  putting it in `VIDEO_EXTS` would make a scan of a music library open every track and print an
  error per file.
- **Extension dispatch falls back to content sniffing only on error.** `container::demux` picks a
  backend by extension and returns immediately on success — sniffing never runs on the happy path
  (no latency cost). If the extension-matched backend *errors* (e.g. a TS misnamed `.mkv`),
  `sniff_demux` re-probes by magic bytes and is adopted only if a sniffed backend actually
  succeeds; otherwise the original, more specific error is surfaced.
- **A leading `00 00 01` is not evidence of Annex-B, and nothing may treat it as such.** The
  three-byte prefix is shared by H.264/H.265, MPEG-1/2 and MPEG-4 Part 2 video, and the MPEG system
  layer, so `container::classify_start_code` routes on the byte *after* it: H.264/H.265 open their
  NAL header with `forbidden_zero_bit`, which must be 0, while every MPEG start code sets bit 7, so
  `>= 0x80` refutes Annex-B structurally rather than heuristically. Only the sub-`0x80` space needs
  validating, and `looks_like_nal_header` is deliberately permissive there (measured: roughly 98% of
  a *cut* MPEG stream's low start codes pass), so **three independent guards** stand behind it and
  none is redundant. It ORs an AVC and an HEVC reading and must keep both, since a VPS-first HEVC
  stream survives only on the HEVC arm and an H.264 AUD only on the AVC arm. `hevc::nal`/`avc::nal`
  reject `forbidden_zero_bit` again while splitting Annex-B, though the *length-prefixed* splitters
  deliberately do not: there the container already declared the codec and each NAL's length, so no
  start-code scan can mint NAL units from unrelated bytes. And `annexb::demux` refuses a head that
  positively classifies as MPEG, which is the only guard covering a **misnamed** file, since one
  reaching the backend by extension never passes the sniffer at all. That last one is not
  theoretical: before it, a retail DVD VOB renamed `.hevc` printed a *fully populated* profile,
  tier, level and bit depth, and under `--full` it still did after the splitter guard landed,
  because `annexb::rescue_sps` hunts the whole file and one of a DVD's tens of thousands of slice
  start codes eventually decodes as an SPS header whose payload parses. `try_sps` is held to
  H.265 §7.4.2.2 (TemporalId 0) for the same reason. Deciding a mid-file MPEG cut properly needs a
  whole-head start-code census (ffmpeg's `mpegps_probe` thresholds), which belongs with a program
  stream backend, not in the sniffer.
- **Stdin input (`hdrprobe -`) is head-only, sniff-dispatched, and lives entirely in main.rs.**
  `process_stdin` reads a bounded head into a heap buffer (`read_stdin_head`: a 64 KiB sniff
  block, then the format's budget + 1 byte — the extra byte is how truncation is detected) and
  feeds the ordinary slice pipeline via the shared `assemble_report` (the back half of
  `process_file`); dispatch goes through `container::demux(Path::new("-"), ..)`, whose unknown
  extension falls to `sniff_demux`. The budget is format-aware: `container::sniffs_as_ts`
  (deliberately the same ordered checks as `sniff_demux` — keep them in sync) selects
  `ts::HEAD_SCAN_BYTES`, everything else gets `STDIN_HEAD_BYTES` (16 MiB, ≥ the 8 MiB raw head
  walks). EOF within the budget ⇒ the input is complete and reports exactly like a file probe
  (no flag); past it ⇒ `Report::input_truncated` plus `suppress_prefix_derived_facts`, a
  post-demux fixup in main.rs keyed on the `Demux::container` label — **never thread a
  truncation flag into backends**: it drops the TS PCR-span duration, the MPEG-PS PTS-span
  duration (both are head-to-tail derivations, and a prefix's "tail" is just the cut point —
  every label `ps.rs` can emit is in that match, so renaming one means editing here too) and
  every non-MP4 bitrate
  (MP4/MOV `video_stream` rates are stsz/trun table sums, exact over any prefix; MKV/MP4
  declared header durations stand). Skipped for stdin by construction: the sidecar gate
  (extension-based), mmap, all prefetch, the ISO branch, and `--full` (a per-file error — a
  pipe has no seekable whole; sibling path args still process). Accepted edge limits, do not
  "fix" without a backend signal: a truncated MKV prefix holding < `mkv::HEAD_SPAN_BYTES` of
  blocks ends its walk without `stopped_early`, so the count ÷ duration fps fallback can read
  low; a truncated fMP4's summed-trun fallback duration describes the buffered fragments only.
  Consumer contract (budgets, suppression table, the broken-pipe-is-success convention) is
  documented in `docs/SCHEMA.md` and `docs/INTEGRATION-STDIN.md` — keep all three in step.
- **Every independent video track is reported; a DV BL+EL pair is one logical track — and the
  classification is per-stream signalling, never a track count.** `Demux::tracks` holds one
  `TrackDemux` per *reported* track (report order: MKV by TrackNumber, MP4 by trak order, TS by
  program then PID — `parse_psi` walks the whole PAT, so a multi-program capture reports one
  track per service with `program` set; the JSON is `video_tracks[]`, the text renders one
  track-rule group per entry with the group's *body* indented by `render.rs::TRACK_INDENT`
  (carried on `Colorizer::indent`: section rules shift right, lead with a `└─` branch marking
  them children of the track rule — colour-only, plain mode has no rule glyphs — and keep
  their right edge flush with the full-width track rule; kv rows deepen past the base gutter,
  and the reflow value column follows the shift — so which sections belong to which track
  reads at a glance), and `-q` prints one line per track tagged `[k/N]` when N > 1 —
  single-track output is byte-identical everywhere, including its geometry: only the
  multi-track arm ever sets a nonzero indent). A second video track/PID is a DV
  enhancement layer **only when its own config says so**: an MP4 trak / MKV TrackEntry whose
  dvcC has `bl_present == 0`, or a TS PID whose 0xB0 descriptor says `bl_present == 0` (its
  `dependency_pid` names the BL PID it folds into). **A present 0xB0 descriptor is authoritative
  and the stream type is not consulted**: §7.1.2 of the Dolby TS spec signals a legal single-PID
  Profile 5 stream with PES-private `stream_type` 0x06 (its BL isn't SDR/HDR compliant, so it may
  not claim 0x1B/0x24) *and* `bl_present_flag == 1`, so that PID is a base layer wearing a private
  stream type. Only with **no** descriptor is the shape inferred — a DV-flagged PID carrying no
  video stream_type is the bare EL/RPU PID. (Inferring it regardless was benign for a lone PID,
  which the "only EL-shaped PIDs" fallback rescued, and merged two tracks into one when a sibling
  video PID shared the program.) Such an EL folds into its base layer's track — chunks
  concatenated so the RPU is scanned, dvcC donated, per-track `dv_dual_track` set, rendering the
  `Structure` line's `Dual track, dual layer` (still gated behind `el_present` via
  `structure_str` in `levels::{finalize,container_only}`); anything else is an independent
  track and never pollutes a sibling's scan or inherits its verdicts. One deliberate exception:
  a TS **program with no DV descriptor anywhere** and >1 video PID keeps the historical BDMV
  rule — an untouched Blu-ray P7 M2TS signals DV via the playlist, not the PMT, so its BL+EL
  PID pair is one dual-track group. Legacy Profile 4 interleaves its EL in one PID/track and
  stays `Single track, dual layer` (corpus `dv4_hevc.ts`). Latency: single-track files keep the
  identical I/O and call sequence; a multi-video TS scales its head packet budget by group
  count (capped, `ts::HEAD_BUDGET_MAX_SCALE`) while the prefetch head warm stays
  `HEAD_SCAN_BYTES` — the overflow on those rare files is a bounded cold read. Under `--full`
  every walk stays single-pass: the MKV/TS streamers fan blocks/AUs into per-track lists in one
  walk, and `sample.rs` aggregates per track (`Scan::tracks` parallels `Demux::tracks`;
  mmap-backed multi-track files scan one merged file-ordered pass over `select_track_chunks`,
  the same selection `prefetch::warm_sample_chunks` replays) — never one pass per track, never
  a parallel reduce.
- **The DVB form of CCID 4 is defined by an SEI, so inference reads the *effective* colour.**
  Dolby's Table 2 gives CCID 4 a second row for Profile 8, transfer characteristic 14 — but the
  defining sentence (v1.5 p10) is "a transfer characteristic VUI value of 14 ... (and optionally
  1, 6, or 15) ... **when used with the alternative_transfer_characteristic SEI message, at every
  random access point, with the preferred_transfer_function set to 18**". The SEI is
  constitutive: 14 is chosen precisely so an SDR receiver reads the stream as SDR BT.2020, and
  the SEI is the only thing that says otherwise, so a bare 14 is an ordinary SDR wide-gamut
  curve and must resolve to *nothing*. `ccid::vui_rows` therefore omits the DVB row entirely
  (it stays pinned by a test as documentation) and `main.rs` hands `fill_inferred_compat` the
  colour with the SEI override already applied — a real DVB stream then reads transfer 18 and
  matches the ARIB row, the same answer by the same table, and all four transfers the spec
  admits for the variant are covered by one rule. Reading the raw VUI instead resolved a bare
  14 to `8.4`, printing a report whose Color line said SDR while its Format line said HLG.
- **Profile number authority.** libdovi's `dovi_profile` can't express AV1 P10 (returns 5/8),
  so `levels::finalize` takes the profile number from the container dvcC when present, else 10
  for AV1. Don't trust the RPU's profile field for the number.
- **The compat *minor* digit resolves on four rungs, and the report says which.** The RPU header
  gives the *major* (5/7/8); the minor is `dv_bl_signal_compatibility_id`, which the RPU cannot
  carry at all. `levels::resolve_compat` walks: **declared** (container dvcC/dvvC/TS descriptor,
  or a DV XML's `GenerateProfile` via `DvAggregate::set_compat_id`) -> **spec**
  (`ccid::spec_ccid` — Dolby's profile table fixes exactly one id for P4/P5/P7/P9 and the legacy
  P0-3/P6, so a raw elementary stream with no dvcC still has a *real* id) -> **inferred**
  (`levels::fill_inferred_compat`, a main.rs post-pass because it needs the track's signalled
  `ColorInfo`: `ccid::infer_ccid` matches the VUI against the profile's candidate Table 2 rows
  and answers only when exactly one survives) -> **assumed** (P8's ecosystem `8.1` convention,
  and *only* P8's — P10/P20 admit several ids with no convention, so they keep a bare major).
  The rung lands on `model::compat_source`; the old boolean `profile_compat_assumed` is gone.
  **The `assumed` rung fills the label only** — `bl_compatibility_id` and `compatibility` stay
  `None`, keeping the older invariant that an absent compat nibble reads as unknown and never as
  a guessed value. Everything else does fill them, which is why a raw P5 now reports `5.0`/id 0
  and a raw P10 HDR10 stream reports `10.1`/id 1 in **JSON as well as text** (the old
  `render::dv_profile_display` completion is deleted; the renderer states no opinion about
  profile digits or colour any more). The metadata-only gate is likewise gone: `assumed`
  discloses its own lack of evidence, which is what made a separate flag redundant, so a raw
  video P8 whose VUI can't separate CCID 1/2/4 is `assumed` for the same reason a sidecar is. The **text report still drops the Profile line for metadata-only
  sidecars** (`render.rs`) — an RPU is profile-agnostic and a DV XML's `GenerateProfile` is an
  authoring target, so a rendered profile would read as a fact the metadata doesn't carry. The
  P7 spec rung also covers the common *video* case of an untouched BDMV M2TS, which has **no
  `0xB0` DV descriptor at all** — Blu-ray signals DV via the HDMV registration descriptor and
  the playlist STN table; only remuxes (tsMuxeR etc.) add the descriptor.
- **The base-layer colour a profile defines is filled into the model, with per-field
  provenance.** `ColorInfo` is omitted per field when *neither signalled nor defined*, and
  `model::ColorSources` says which of `container`/`stream`/`sei`/`spec` produced each value it
  does carry (per field, not per object: a P5 stream signals its range and derives the other
  three). `levels::fill_derived_color` is a main.rs post-pass alongside `fill_derived_level`,
  video inputs only, and has **three** hard gates. **Signalled always wins** — the fill only ever
  touches an absent field, so the corpus's declared 8.4 signalling full range against the
  table's limited keeps `full`. And it runs on a **declared or spec id only, never an
  inferred one**: an inferred id was deduced *from* this colour description, so filling it back
  would launder a deduction into three fields that read as facts (and would add nothing — an
  inferable id implies the signal was largely present). Third, it requires a **base layer**
  (`bl_present`): a Table B row describes a base layer's VUI, and a P4 EL's own VUI is
  byte-identical to a P5 base layer, so stating it over an EL-only track would describe a
  stream that is not there. `dolby_vision.pq_reshaping` deliberately
  does *not* share that gate: it fires on a resolved CCID 0 from any rung, because it is new
  information rather than a back-fill, and Dolby's footnote keys on exactly that condition.
  Order matters in `main.rs`: `hdr::assemble` runs on the *demuxed* colour before any of this,
  so nothing derived can feed back into classification. **"Absent" has to mean *unsignalled*,
  not merely unnamed.** A `ColorInfo` field is `None` both when the source carried nothing (or
  the explicit "unspecified" code 2, which is the case the fill exists for) and when it carried
  a real CICP code no shared table names — and filling the second overwrites a genuine signal,
  then labels it `spec`, i.e. "not signalled anywhere". So **every** colour producer builds
  `ColorInfo` and `ColorSources` together through `container::color_from_cicp`, the one place
  that still sees the raw codes, and an unnamed code is marked `ColorSource::UnnamedCode` for
  the fill to skip. That marker is internal: `model::hidden` keeps it out of the report, so the
  documented guarantee that `color` and `color_source` carry the same key set still holds. The
  `cicp_*` tables name every code H.273 defines, which keeps this class small — it now covers
  only reserved values and code points added to the standard after this build — but *small is
  not empty*, and the failure mode is silent: a wrong value reported as though nothing was
  signalled. There is deliberately no constructor deriving provenance from a finished
  `ColorInfo`; it could not make the distinction, and a caller using one would relabel fields
  it never wrote. **The HEVC/AVC SPS parsers keep
  `video_full_range_flag` even when `colour_description_present_flag` is 0**, decoding the
  three CICP values as the 2 (unspecified) that H.264/H.265 Annex E infers for them: a stream
  may legally declare full range and nothing else, and dropping the flag with the description
  would let the spec fill state the *opposite* range as though nothing had been signalled.
- **AVC (Profile 9) RPU is found by *content*, not by NAL number.** The DV RPU rides in an H.264
  *unspecified* NAL (Dolby uses type 28; the range is 24..=31), payload = the RPU EBSP beginning
  with the `rpu_nal_prefix` byte `0x19`. `sample.rs` treats an unspecified-range NAL as an RPU only
  when `payload[1] == 0x19` **and** libdovi validates it (CRC): so an atypical mux using another
  unspecified type still parses, and a non-DV unspecified NAL is never misread. libdovi has no
  AVC entry point, but its parsing is codec-agnostic once the header is off — `dv::rpu::parse_avc_rpu`
  strips the **1-byte** AVC header, clears emulation prevention (`bits::ebsp_to_rbsp`), and calls
  `DoviRpu::parse_rpu` (which locates the `0x19` prefix). Don't route AVC through
  `parse_unspec62_nalu` — that strips a **2-byte** HEVC header. **Codec authority:** MP4 from the
  sample entry (`avc1`/`avc3`/`avc2`/`avc4`/`dva1`/`dvav` → `Codec::Avc`; `avc2`/`avc4` are
  AVC2SampleEntry, which the Dolby ISOBMFF spec lists beside `avc1`/`avc3` as a dvcC container),
  MKV from the `V_MPEG4/ISO/AVC` CodecID
  (CodecPrivate is an `avcC`; `parse_avcc_record`'s embedded SPS supplies depth/chroma/profile —
  also what gives an SDR AVC MKV its 8-bit / Hi10P 10-bit report), TS from PMT `stream_type`
  (`0x1B` AVC vs
  `0x24` HEVC), falling back to DV profile 9 ⇒ AVC only when no video `stream_type` is present (a
  bare DV/EL PID). P9 has no EL and an SDR base (CCID 2 ⇒ `SDR` in `hdr::assemble`, the
  same branch Profile 4 uses); its Rec.709 VUI (`0,1,1,1,0`) collapses to a single `BT.709` label
  because primaries == transfer (unlike P5, whose encoding differs from its colour space).
- **An unrecognized sample-entry FourCC silently costs the whole dynamic report.** `Codec::Other`
  has no arm in `sample.rs`, so the track is never scanned for RPUs: container facts survive
  (`dvcC`/`hvcC`/`avcC` parse by box type) while every sampled DV level, the trim set, the cadence
  verdict and the EL type vanish, and the codec renders as the raw FourCC. The MP4 sample-entry
  match is therefore a correctness surface, not a cosmetic one. Two forms feed it that aren't the
  obvious four: **`avc2`/`avc4`** (AVC2SampleEntry — the Dolby ISOBMFF spec's §3.1 container list,
  §8.1.1 and box hierarchy all name it as a `dvcC`/`dvvC` container beside `avc1`/`avc3`), and
  **`encv`**, the ISO/IEC 23001-7 common-encryption form, whose real FourCC is preserved in
  `sinf`/`frma` and recovered by `mp4::original_format` *before* the codec match runs (both Dolby
  streaming specs state the substitution outright). Reading an encrypted track is sound rather than
  opportunistic: those same specs require the NAL length fields, the `nal_unit_type` bytes and the
  **whole RPU** to stay unencrypted, so only slice payload is ciphertext, and `sinf` is an *added*
  child so the original `hvcC`/`dvcC` sit where the walk already expects them. An incomplete chain
  (no `sinf`, no `frma`, a truncated `frma`) stays on the fallback rather than guessing a codec.
  Only `encv` is unwrapped — `enca`/`encs`/`enct` are media types this backend never reports.
- **A DV enhancement layer folds by its `tref`/`vdep` reference, not by picture size.** §8.2.2 of
  the Dolby ISOBMFF spec requires a dual-track file to name the dependency there, and it is the
  exact analogue of the TS backend's `dependency_pid` — keep both carriage paths resolving the
  fact the same way. ELs bucket per target, so a mux with several base traks routes each residual
  by what it references. The widest-independent-trak heuristic stays as the fallback (plenty of
  muxes omit `tref`) but must never outrank a resolvable reference, and a `vdep` naming a track the
  file lacks falls back rather than dropping the EL. `vdep` carries no count field, so its entry
  total is implied by the box extent and bounded by `MAX_TREF_REFS`.
- **`--full` changes demux behaviour, not just sampling.** It threads into `container::demux(..,
  full)`: TS streams the whole video ES through the sampler in bounded `ts::STREAM_WINDOW_BYTES`
  windows — demux itself stays a head-window metadata pass, plus an SPS-rescue walk only when the
  head held no SPS at all (vs the default's single head window of `ts::HEAD_SCAN_BYTES`),
  **MKV streams like TS** — demux keeps the default's bounded head
  walk (`HEAD_SPAN_BYTES`) and exposes `Demux::mkv_stream` (`mkv::MkvFullStream`); `sample::scan`
  drives the resumable `mkv::BlockStreamer` cluster-by-cluster in `mkv::STREAM_SPAN_BYTES`
  windows, extracting each window's blocks as they are discovered, so index and scan are **one
  fused pass** (a remote file crosses the wire once at any size — never reintroduce a demux-time
  exhaustive cluster index; on a >RAM remux that made the scan pass re-transfer the file). The
  exact block byte/frame totals the old index computed come back on `sample::Scan::{es_bytes,
  frame_count}`, applied in main.rs (bitrate fills only when the statistics tags didn't;
  fps count÷duration only when `DefaultDuration` didn't) — and **raw HEVC/AV1 fuse the same
  way**: demux keeps its bounded head walk (`annexb::HEAD_SCAN_BYTES` / `av1::HEAD_SCAN_BYTES`,
  8 MiB) on every path and sets `Demux::raw_stream` (`container::RawFullStream`);
  `sample::scan_raw_full` drives the format's whole-stream walk (`annexb::walk_aus`,
  `av1::walk_obu_tus`, `av1::walk_ivf_frames`), extracting each `AGG_BATCH` of completed AUs
  right behind the walk front, so the file is read once at any size (the old shape split the
  whole stream in demux and re-read every AU in the scan — two wire transfers on a >RAM remote
  file). What the demux-time exhaustive walk used to compute comes back on the `Scan`: raw
  AV1's exact frame count and duration (`Scan::{frame_count,duration_secs}`) and IVF's
  whole-stream average fps (`Scan::fps`), applied in main.rs only where demux left the field
  `None`. Two rescue walks remain demux-time, both rare: raw HEVC scans forward for an SPS only
  when the head window held none (early-exits at the first parsable hit, resuming at the
  boundary of the NAL the head window cut — mirroring TS's `sps_rescue`), and raw OBU falls
  back to the old exhaustive demux walk only when the head held no sequence header (near-dead:
  the sniffer requires a TD/SEQ first OBU, and OBU has no resync marker so a bounded mid-file
  rescue can't exist). Keep new backends
  consistent — bounded by default, fused single-pass under `--full`. A backend
  that bounds its `chunks` index must not derive fps/frame-count from `chunks.len()` in the bounded
  path. **The bounded default is always head-only, never a spread of mid-file windows**: every
  format reads a minimal head region to fill the fields, `[sampled]` tags flag what could vary
  per-title, and mid-file variation (e.g. L5 aspect changes) is `--full`'s job by design. For raw
  AV1 head-only is also forced (low-overhead OBU has no byte-scannable sync marker — AV1 has no
  emulation prevention, so a temporal-delimiter byte pattern can occur inside frame payload — so
  the demux can only resync from the byte-0 boundary); raw HEVC *could* resync on start codes, but
  a window spread costs ~50 MiB of reads on a NAS (measured ~600 ms at 1 GbE) for coverage that
  was never the default's contract — don't reintroduce it. **Frame
  rate for boxless containers comes from an in-band constant-rate signal, independent of the bounded
  sample, so it's correct by default**: TS/M2TS and raw HEVC from the SPS VUI timing info
  (`vui_time_scale / vui_num_units_in_tick`, halved when `field_seq_flag` marks fields, parsed in
  `hevc::sps`); **raw AV1 OBU** from the sequence header's `timing_info()` (`av1::seq`), present only
  when `equal_picture_interval` is set — AV1 encoders usually omit it, so this is `None` far more
  often than HEVC. **IVF** is the one exception that derives fps from per-frame PTS (a sampled average,
  so it can drift a hair under bounding vs `--full`). MP4/MKV take fps from their container timing.
  `None` when the signal is absent, never a guess. **Duration for raw AV1** = frames ÷ fps: OBU has
  no frame-count record, so it's known only when the whole stream was walked (`--full` or a small
  file) *and* fps is known; IVF reads its total frame count from the file header, so duration survives
  the bounded walk when the muxer filled that field.
- **TS/M2TS default reads to the *first IDR*, not byte 0.** TS carries no container box, so
  resolution/colour/frame rate come only from the in-band SPS — which rides the first IDR, typically
  ~one 4K GOP (~10 MiB) in. So `ts::head_reassemble` is a *single* head window whose only bound is
  a packet budget sized to `HEAD_SCAN_BYTES` (24 MiB, ~2× the observed SPS depth), so the read
  isn't cut short before that IDR. Don't "optimize" this down to a few MiB (drops resolution/colour, and L5 falls
  back to raw offsets) or reintroduce the old whole-file window spread (defeats the remote win).
  **Duration is the one exception that also reads the tail:** TS has no duration box, so — like
  MediaInfo — it comes from `last_PCR - first_PCR` on the PCR PID. The first PCR is free from the
  head window; the last comes from a *bounded* trailing window (`ts::TAIL_SCAN_BYTES`, 4 MiB). Head
  + tail only, never the middle. A discontinuity flag in the sampled tail, a missing PCR, or an
  implausible span yields `None` rather than a wrong number (`ts::pcr_duration`).
- **A program stream's duration is the video PTS span plus one frame, and the SCR is not a
  fallback.** The span itself is one frame short by arithmetic, not approximation: frame `k` of
  `N` is presented at `start + k/f`, so first-to-last is `(N-1)/f` while the stream occupies
  `N/f`. `ps::whole_frame_duration` adds the interval when the frame rate is known (nothing to
  add without one, and the bare span stands), which reproduces MediaInfo **exactly** on all four
  corpus program streams — including the overall bitrate to the byte on three of them, since the
  rate divides by this. The span is `ps::pts_span`: smallest PTS in the head window to largest in
  the tail, min/max rather than first/last because a PTS is a presentation time and B-frame
  reordering makes it non-monotonic in stream order; when the two windows *meet* a timestampless
  tail falls back to the head's own maximum (they have read the whole file between them), and
  when they don't it must not, or the answer would describe the head window alone.
  The **pack clock is never a duration**: it times byte *arrival*, so it closes when the mux ends
  rather than when the last picture is shown, measured at −11.0% on the 2 s corpus clips, −0.47%
  on the 60 s retail DVD and **+3.2%** on an ffmpeg-muxed VOB — neither the size nor the
  direction of the error is predictable, which is also why there is no span-comparison
  cross-check: any tolerance wide enough to accept that spread catches nothing. What the SCR *is*
  for is spotting a clock reset, the one way a plausible-looking span can be wholly wrong — a
  backward step inside either window, or a tail whose clock starts before the head's ended.
  **That second check is partial and the limit is structural**: it catches `cat a.vob b.vob` only
  when the appended segment is short enough (~17 s at DVD rate) that its tail clock still sits
  below the head window's last; append more and both windows are internally monotonic, each
  wholly inside one segment, and the join is invisible — ffprobe and MediaInfo report the same
  wrong number there, so it bounds head-and-tail probing rather than this backend. The reverse
  order and any join inside the head window *are* caught. `None` also covers an absent timestamp,
  a span over 26 h (the 33-bit clock's own range, one wrap being handled) and a non-positive one.
  **The two windows must stay disjoint** (`tail_start.max(head_end)`): overlapping them makes the
  tail's clock start before the head's by construction, so the reset guard fires on every
  ordinary file in the 8–12 MiB band and silently drops its duration *and* its bitrate.
  One more comparison earns its place, and it is the *same* one that fails against concatenation:
  within a single window the two clocks measure the same bytes, so a presentation span exceeding
  the arrival span by more than the decoder buffer delay is impossible rather than merely odd
  (real files: at most 0.2 s over; `Walk::pts_within` allows half again or two seconds). That is
  what stops one stray timestamp from setting the answer — the span is a min/max over a window,
  unlike the transport backend's first-and-last pair, and a single non-conforming packet was
  measured turning a real file into "25 h 55 m at 1.26 kb/s". The distinction is worth keeping
  straight: the cross-check catches an *impossible* span, never a plausible-but-wrong one.
  Bitrate is `overall` scope only (the byte count includes audio and packet
  overhead) and `None` with more than one *reported track*; **`program_mux_rate` is never read for
  it** — H.222.0 §2.5.3.4 defines it as a per-pack ceiling that "may vary from pack to pack",
  and a real retail DVD writes 32964, i.e. 13.2 Mbit/s against an actual 6.6.
- **The sampler always pins the SPS-carrying AU (`Demux::sps_chunk`).** Per-GOP prefix SEIs (HLG
  alt-transfer, ST.2086 mastering, CLL) ride only RAP access units, and a TS capture (or a raw ES
  cut) often starts mid-GOP: chunk 0 is then a pre-IDR picture and the sparse sample spread rarely
  lands on one of the few RAPs, so those SEIs were silently missed (corpus-external repro: an HLG
  broadcast capture classified SDR, because broadcast HLG is signalled *only* by the alt-transfer
  SEI over a BT.2020-10 VUI — MKV/MP4 don't hit this since their chunk 0 is a sync sample by
  construction, which is exactly why the same file remuxed to MKV read correctly). The TS and
  raw-HEVC backends record the chunk whose SPS filled the metadata fields and
  `sample::select_indices` inserts it into every sampled set; `prefetch::warm_sample_chunks`
  replays the same call with the same `sps_chunk`, so the warm stays aligned with what the
  sampler faults.
- **NAS speed rides on the prefetch warms, and warm regressions are silent.** Everything here is
  timing-only: tests pass and `-q` is unchanged when it breaks; the regression only shows on a
  real network path. Warming is gated by `prefetch::is_remote`, decided from the open handle
  (Windows `FileRemoteProtocolInfo`), never by re-probing the path (a `canonicalize` re-opens
  the file over SMB). Two stages, both executed by `prefetch::warm_ranges` (sort, coalesce
  overlaps, then concurrent positioned reads so one range's latency hides another's):
  `warm_metadata` before demux gathers the head window sized to what the front parse actually
  consumes (`ts::HEAD_SCAN_BYTES` for TS; the small `MP4_HEAD_WARM` for a confirmed ISOBMFF and
  `MKV_HEAD_WARM` for an MKV whose first-cluster offset resolved — both have their real regions
  warmed by exact extent, so a generic multi-MiB head would only stream bytes nothing parses,
  ~80 ms of pure transfer per 8 MiB at 1 GbE; the generic `HEAD_WARM` otherwise, which also
  covers the raw HEVC/AV1 bounded head walks whole), the TS tail window, the
  `moov` extent, the MKV `Tags` extent plus the head *block* window from the first cluster
  (SeekHead-resolved via `mkv::head_blocks_extent`, so attachments before the clusters can't
  push the block walk past the warm), and fMP4 fragment heads from a front `sidx`
  (`mp4::sidx_fragment_heads`) or, failing that, the tail `mfra` random-access index
  (`mp4::mfra_fragment_heads`, found in O(1) via the trailing `mfro`);
  `warm_sample_chunks` after demux replays
  `sample::select_indices` over the container's exact chunk ranges so the sampler's scattered
  AU faults arrive warm — it skips ranges inside `warm_metadata`'s return, the *coalesced*
  contiguous warmed prefix from byte 0 (an MKV head that merges into its block span counts
  whole). The chunk warm is skipped under `--full` (every chunk is read anyway; its `--full`
  counterpart is the `Frontier` below), under `--no-rpu` (no chunk is *sampled* — the demux-time gap-fillers still read up to 32 chunks for the codec's own headers, which is where an AVI's whole cost lives), and for TS
  (chunks index into `reassembled`, not the file). **`--full` on a strict-remote volume
  tailgates `prefetch::Frontier`**, a bounded look-ahead warm riding the progress-tick sites:
  each whole-file walk calls `ensure(pos)`/`ensure_to(end)` so the file crosses the wire once,
  linearly, instead of thousands of scattered fault round-trips. The bytes land in the OS page
  cache only (owned heap unchanged), the look-ahead is capped (`FRONTIER_AHEAD`, with exact
  known spans — an MKV cluster, a scan batch, a TS window — warmed whole since they're consumed
  immediately), and the frontier is monotonic per file. Every container is single-pass under
  `--full` (fused or moov-indexed) — MKV/TS stream in windows, MP4 scans its moov-indexed
  chunks in file order, and raw HEVC/AV1 *and FLV* fuse their whole-stream walk with extraction in
  `sample::scan_raw_full` — so one transfer covers any file size; the only whole-file demux
  walks left are the rare metadata rescues (no SPS / no sequence header in the head window).
  Gating is `is_remote_strict`, not `is_remote`: the plain verdict errs remote off-Windows
  (fine for cheap bounded warms), the strict one errs local (Linux resolves
  `/proc/self/mounts`, macOS/FreeBSD the `getmntinfo(3)` table — the same longest-prefix
  matcher, `network_mount`, on a different feed; BSD FUSE mounts stay local since
  `f_fstypename` names the driver, not the backing fs; unknown platforms decline) because a
  forced linear read of a local disk would regress. TS windows and heap-buffer chunk lists never touch the frontier with buffer
  offsets — only real file positions go in. The `sidx`/`mfra` ranges are a **hint
  only**: the fragment index is always built from the `moof` boxes themselves, so a wrong or
  missing index wastes a warm but can never change output. Couplings that remain numeric and easy to break
  silently: **raw AV1 and raw HEVC** — `av1::HEAD_SCAN_BYTES` (the bounded head walk for both
  OBU and IVF) and `annexb::HEAD_SCAN_BYTES` (the bounded head NAL scan) both
  <= `prefetch::HEAD_WARM`, so the generic head warm covers the whole walked span; **TS/M2TS** —
  the warmed head (chosen by `looks_like_ts`) is exactly `ts::HEAD_SCAN_BYTES`, the demux's
  packet budget is sized to stay within it (`HEAD_SCAN_BYTES / 192`, the larger stride), and the
  warmed tail is exactly `ts::TAIL_SCAN_BYTES` for the last-PCR duration read; **MPEG-PS** —
  the same head/tail pair, `ps::HEAD_SCAN_BYTES` <= `HEAD_WARM` (so the generic head covers the
  walk, no branch needed) and the warmed tail exactly `ps::TAIL_SCAN_BYTES` for the last-PTS
  read, routed by a `looks_like_ps` whose content half is **byte 0 only** — a pack start code
  plus a valid discriminator, never the head census `ps::demux` runs, because faulting a
  megabyte in *before* the warm is the round-trip storm warming exists to prevent; **FLV** —
  the same shape again, `flv::HEAD_SCAN_BYTES` <= `HEAD_WARM` so the generic head covers the
  bounded tag walk, plus exactly `flv::TAIL_SCAN_BYTES` for the duration fallback that follows
  the file's final `PreviousTagSize` back to the last tag; **ASF** needs no entry at all, its
  Header Object being at byte 0 and tens of KiB at most; **MKV without a
  Cluster SeekHead entry** falls back to the old handshake, `prefetch::HEAD_WARM` >= the first
  block's offset + `mkv::HEAD_SPAN_BYTES` (with a resolved cluster the coupling is structural:
  `MKV_HEAD_WARM` holds only the front metadata, and the block span is warmed by exact extent).
  Warm via a positioned `ReadFile`/`read_at`, **not**
  `Mmap::advise` (memmap2's advise is `#[cfg(unix)]`, a no-op on the Windows/SMB target).
- **Malformed-input safety in `mp4.rs`.** `read_u32/u16/u64` are bounds-safe (return 0 on OOB);
  any box-declared count fed to a loop/alloc must go through `clamp_count`. Apply the same
  discipline to new table parsing. **`iter_boxes` additionally rejects a box whose declared size
  undercuts its own header** (a 32-bit `size` of 2..=7, or a 64-bit `largesize` under 16), which
  would make `payload > end`: every consumer slices `payload..end`, so admitting one *panics*
  rather than erroring, and a panic is exit 101, outside the tool's 0/1/2 contract, which in a
  directory scan aborts the whole run and prints nothing at all. The guard belongs in the walk,
  not at the call sites: it was originally spot-checked at two of seven and the other five were
  live crashes. Keep new sample-entry children on the walk rather than re-deriving extents.
- `split_annexb` treats the buffer start as an implicit NAL boundary (chunks begin at a NAL
  header, not a start code) — relied upon by the length-prefixed and head-window paths.
- **Average bitrate is per-backend and correct-or-labelled, never a wrong number.** Each backend
  fills `Demux::bitrate: Option<Bitrate>` (`model::Bitrate::{video_stream_bps,video_stream,overall}`)
  so container quirks stay local. A *video-stream* rate is emitted only from an exact source: MP4
  sums the `stsz` sizes (exact, free — sample tables, never sample data; an fMP4 sums the `trun`
  sizes over the summed `trun` durations instead, and an *empty* index yields `None`, never 0 b/s);
  MKV prefers the mkvmerge
  `BPS` statistics tag (what MediaInfo reports — used verbatim since it already spans the video
  track's own duration, which the Segment duration only approximates), else `NUMBER_OF_BYTES`, else
  the summed block index *only when complete* (`!stopped_early`); TS under `--full` sums the
  streamed completed-AU bytes (`sample::Scan::es_bytes`, applied as the report's rate in `main.rs`
  since the total exists only after the streaming scan — demux leaves `bitrate` unset on that
  path, and `Some(0)` bytes still yields `None`, never 0 b/s; `--full --no-rpu` still walks the
  stream count-only so the exact rate survives). **AVI sums its own index** — `idx1` on a
  single-segment file, the OpenDML `ix##` chunks otherwise — over the *video stream's* declared
  duration, which reproduces MediaInfo's video rate byte-for-byte on eight of the nine corpus
  files; and it emits **no rate at all when the first RIFF segment declares more bytes than the
  file holds**, because a truncated AVI's numerator is the bytes present while its denominator is
  the whole declared runtime. That case is not theoretical or cosmetic: a 5 MiB file cut inside
  its own `idx1` reported 331 kb/s *labelled as an exact video-stream rate* against a true 752,
  since a cut index reads as a shorter well-formed one. MediaInfo reports the same wrong number;
  ffmpeg instead rescales the *duration*, which is a guess about where the cut landed. The
  declared duration is a header fact and stands. Otherwise an *overall* rate (file length ÷
  duration, labelled distinctly because it
  counts audio + overhead) or `None` (no duration: raw HEVC/AV1). Never divide a bounded head-window
  index by the full runtime. **MKV reads the statistics `Tags` via one bounded tail seek**: mkvmerge
  writes `Tags` after the clusters, past the head window, so the demux follows the front SeekHead's
  Tags pointer (`seekhead_tags_offset`) and parses just that small element (`parse_tags_at`). This is
  the *only* place the MKV default path touches the tail — a single bounded read, warmed on NAS by
  `prefetch` (which resolves the same extent via `mkv::tags_extent` and streams it alongside the head,
  mirroring the TS tail-PCR warm; keep the two in sync). Under `--full` the walk reaches `Tags`
  naturally. A track may carry several `Tag`s for one UID (e.g. SOURCE_ID before the statistics), so
  select the first entry with a usable value, not the first UID match.
- **A closed stdout is a success signal, not an error.** `hdrprobe … | head` and `| less` (quit
  early) are ordinary use, and `print!` *panics* on the write failure they produce — a Rust
  backtrace over the user's terminal and exit 101, a code outside the tool's contract (0 ok,
  1 usage, 2 unreadable) and outside the fuzz gate's asserted `{0,2}`. Every report write
  therefore goes through `main::write_stdout`, which treats `ErrorKind::BrokenPipe` as the
  consumer having read its fill: the streaming loop stops scanning (nobody is left to read the
  remaining reports), the buffered tail write is skipped, and the run exits 0. This is the same
  convention the *stdin* path documents from the other end — there hdrprobe is the reader and
  the upstream writer sees the broken pipe, which `docs/INTEGRATION-STDIN.md` already calls the
  normal success signal. Don't reintroduce a bare `print!` on the report path.
- **Progress is `--full`-only, stderr-only, and single-threaded by design.** `main` resolves
  every `--progress` mode to `Off` unless `--full` is set (the fast path never reports), and
  nothing progress-related may ever write to stdout — SCHEMA.md promises stdout is the pure
  report stream, and the corpus byte-identity gate implicitly checks it. Reports *stream*: each
  file's report goes to stdout the moment that file finishes (text, quiet, and NDJSON; pretty
  JSON still waits for its one closing array and `--output` keeps its single file write) — the
  streamed bytes are exactly what the old end-of-run dump printed, so every piped/machine stream
  is byte-identical, and the per-file `finish_erased` (the erase is stderr-side cursor movement,
  never stdout bytes) fires only on the colored interactive text path, the same gate as the
  masthead — never for quiet/JSON/piped/`--output` runs or after an error. The sink
  (`progress::Progress`) holds `Cell` state on purpose: every tick site is single-threaded —
  demux walk loops, the TS window loop, and `sample::scan_chunks`' batch boundary *between*
  rayon collects — so never hand it into a `par_iter` closure. `update` is byte-gated before it
  is clock-gated (one `u64` compare in the common case, `Instant::now()` at most once per gate
  step); keep new tick sites on that pattern, and keep `Off` free — every default-path call
  runs through it. The JSON contract (SCHEMA.md "Progress events"): a `progress` event's
  percentages are monotonic per phase, the `Scan` phase always closes at 100% (an `Index` walk
  may legitimately end short — never fake its 100%; `Index` now appears only for the rescue
  walks, since every container's ordinary `--full` work is a single fused `Scan`), and `done`
  is emitted only for a file that produced a report. The hot `nal::split_annexb` stays
  tick-free: the no-op-closure monomorphization of `split_annexb_impl` compiles the gate out;
  only `split_annexb_streamed` (the raw-HEVC `--full` fused walk) pays for it.
- **The Color line suppresses the matrix, except where suppressing it would state something
  false.** `render::build_color_line` prints primaries and transfer but drops `color.matrix`,
  because every matrix except Dolby's IPT-PQ-C2 restates what the primaries already said. That
  reasoning fails in two ways, and each has its own escape.
  **(1) There may be no primaries to restate.** With primaries *and* transfer both absent,
  suppressing the matrix empties the line, and the report then reads "nothing was signalled" over a
  stream that signalled something. MPEG-2 is where this became ordinary (ffmpeg's encoder writes
  `colour_description` set with primaries and transfer at the explicit "unspecified" code 2, matrix
  real), but it applies to any track whose only colour signal is a matrix, VP9 and ProRes included.
  **(2) The matrix may name a different system than the primaries.** VC-1's spec defaults are
  BT.709 primaries and transfer over a **BT.601** matrix, and a stream clearing
  `COLOR_FORMAT_FLAG` — every VC-1 file observed — is *defined* to be that, so collapsing the line
  to "BT.709" states the opposite of the matrix half. The comparison is by name (`!m.starts_with(p)`)
  because the labels are the value space, and a matrix whose name extends the primaries' is the
  restatement the rule is about: BT.2020's NCL and CL rows are the only such pair in the CICP
  tables, and **renaming `cicp_primaries(9)` to anything longer than `"BT.2020"` would silently
  start printing a matrix on every HDR file** — the two tables are coupled here and nothing else
  enforces it.
  A matrix shown under either escape is placed **after** the primaries/transfer pair and suffixed
  ` matrix`, because the line's first slot is where a reader parses primaries and matrix labels
  share that namespace (`BT.601 (NTSC)` is `cicp_primaries(6)` as well as `cicp_matrix(6)`, so
  leading with it reads as the exact inverse of the truth). IPT-PQ-C2 keeps its bare leading
  position: it names a colour space no primaries label spells, so it cannot be misread.
  The JSON always carried the matrix; only the text line changed. No corpus file is affected in
  any mode, which is why the byte-identity gate did not move.
- **Value-line reflow is terminal-only and byte-neutral everywhere else.** kv rows longer than
  the terminal wrap at their part separators (trailing ` ·`/`,`/` +`, or the unstyled double
  space before a warning chip — never mid-part, never inside a chip) with continuations
  indented to the row's own value column — `VALUE_COL` plus the track-group indent, so a
  multi-track body's wraps stay aligned under its shifted values (`render.rs::wrap_line`,
  ANSI-aware: a break inside a styled span closes it at the line end and re-opens it on the
  continuation). The width is
  `RenderOpts::wrap_width`, probed once per run by `main::terminal_width` (the stdout console
  window: Win32 `GetConsoleScreenBufferInfo` / unix `TIOCGWINSZ`) and `None` for pipes,
  redirects, `--output`, JSON/NDJSON, and quiet — so every machine-consumed stream, the corpus
  `-q` gate, and any line that already fits stay byte-identical. Below `MIN_WRAP_WIDTH` reflow
  bows out to the terminal's own hard wrap. Never give the probe a fallback guess (`COLUMNS`
  etc.): a wrong width would reflow piped output a consumer expects unwrapped. **Rules follow
  the same probe**: the section rules and the between-reports divider stretch to `wrap_width`
  when it was probed (`Colorizer::rule_width` — full bleed, no cap, and no `MIN_WRAP_WIDTH`
  floor, since a shrunk rule is safe where a shrunk value column isn't) and keep the fixed
  `RULE_W` fallback on every unprobed stream, so piped text keeps its historical 64-column
  divider byte-for-byte. The masthead stays fixed-width — it's glyph art, not a rule.

## Verifying changes

**Review subagents get a resource budget, in the brief, every time.** Parallel finders are worth
their cost on this codebase — hand-rolled byte parsers over untrusted input fail in ways a green
test suite cannot see — but four unbudgeted ones filled 64 GB of RAM and pegged the CPU at 100%,
halting the dev machine. Three workloads stacked: six concurrent `cargo build --release` runs (each
agent forked its own `--target-dir` to dodge the project's lock), concurrent x264/x265 encodes of
1 GB+ fixtures, and ~250,000 process spawns for fuzzing. So: **finders never build** — build once
and hand them the binary path, forbidding `cargo build`/`test`/`clippy` and `--target-dir`; **at
most two at a time**; **fixtures under ~50 MB**, deleted in the step that measures them (a size
regression shows at 50 MB as plainly as at 350 MB — what exposes an unbounded per-chunk walk is
chunk *count*); **fuzz budgets in the low thousands**, since structure-aware enumeration of
declared size/length fields is what finds defects and 170k random runs found nothing 20k had not.
Watch memory while they run, not after.

Cross-check against `mediainfo --Output=JSON` / `ffprobe` / `dovi_tool info` (the ground truth
used throughout). The corpus lives in `testfiles/integration/` (the whole `testfiles/` tree is
local-only and gitignored — nothing under it is committed). For robustness work, byte-mutation
fuzz the release binary over the corpus and assert no `panicked`/exit codes outside {0,2}.

# Fable family (think / act / prove) — HARD GATES, not style guidance
- ENTRY GATE: when a message asks you to investigate, debug, fix, implement, build,
  or change something, invoke the Skill tool with `fable:fable-method` BEFORE your
  first tool call. Following the loop "in spirit" without loading the skill does not
  count. If a turn that started as a question escalates into edits, run the gate
  before the first Edit/Write.
  When a message contains an instruction to present your investigation results, show
  them first, before continuing and await confirmation from the user.
- EXIT GATE: work that produced an artifact (code change, commit, PR/issue/reply
  text, config change) is NOT DONE until a `fable:fable-judge` pass has run on it.
  Do not commit and do not present work as finished before the judge pass.
  "Did that actually work?" = fable-judge.
- Skipping either gate is allowed only for trivial work and only by writing one
  explicit line in the reply: `fable gate skipped: <reason>`. A silent skip is a
  false-completion claim.
- Unattended or subagent-fanout tasks: `fable:fable-loop` instead of fable-method.