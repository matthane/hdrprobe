# CLAUDE.md — working notes for hdrprobe

Fast HDR / HDR10+ / Dolby Vision metadata inspector: one native Rust binary that memory-maps a video
file, demuxes without decoding, samples RPUs, and prints a sectioned report in less than 1 second.
It also parses metadata **sidecar** files (raw DV RPU, DV CM XML, HDR10+ JSON) into the same
report. This file plus the module-level doc comments are the design reference — read the
relevant section and the code it points at before non-trivial changes.

## Commands

```sh
cargo build --release          # binary at target/release/hdrprobe
cargo test                     # 219 unit tests
cargo clippy --release         # must stay at zero warnings
./target/release/hdrprobe testfiles/integration/ -q   # one-line report per corpus file
```

Bar for any change: **zero `cargo build` warnings, zero `cargo clippy` warnings, all tests
pass, and the corpus (`-q`) output is unchanged** unless the change intends to alter it.

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
tests the binary for Windows x86_64, Linux x86_64 + aarch64 glibc + aarch64 fully-static musl
(no libc/loader dependency, for minimal userspaces like CoreELEC/LibreELEC boxes across old
vendor kernels through current), macOS arm64 + Intel (Intel is
cross-compiled on the arm64 runner and tested via Rosetta), and FreeBSD x86_64 (no GitHub
runner exists, so a separate job builds, tests, and packages inside a FreeBSD VM on the Linux
runner — keeping the build-and-test-on-target rule), and attaches the archives plus
`SHA256SUMS` to a **draft** GitHub release for manual review. A `workflow_dispatch` run exercises
the gates and builds without creating a release. The corpus `-q` check stays a manual pre-tag step
(`testfiles/` is local-only). The code is deliberately portable outside `shell.rs`/`prefetch.rs`'s
`cfg(windows)` branches — keep new platform-specific code behind `cfg` with a non-Windows path, and
never parse bytes native-endian.

- `main.rs` — clap CLI, per-file dispatch (sidecar files first, then the Blu-ray ISO branch,
  then the video pipeline), exit codes (0 ok / 1 usage / 2 unreadable).
- `container/` — one hand-rolled demuxer per format: `mp4.rs`, `mkv.rs`, `ts.rs`, `annexb.rs`,
  `av1.rs` (which also owns the IVF wrapper's FourCC dispatch: `VP90` → the VP9 IVF demux,
  `VP80` → an honest error, else AV1); `mod.rs` holds `Demux`/`Chunk`/`DvConfig` and the shared
  dvcC/hvcC/CICP decoders.
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
- `dv/` — `rpu.rs` (libdovi wrapper + panic guard), `levels.rs` (title-stable aggregation).
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
  payloads are never copied up front. **Every container backend is hand-rolled on purpose** —
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
  (compat id 1, or 6 for UHD Blu-ray). Every other base still carries L6 on every frame but it
  is inert there: on IPT-PQ-c2 (compat 0: P5/P20/AV1 10.0) and HLG (compat 4: 8.4/10.4) the CLL
  half is a zeroed placeholder (corpus-verified, including Dolby's own P5 demo and the 8.4/10.4
  samples), and an SDR base signals no static metadata either (the P9 corpus file's *filled* L6
  is not counter-evidence: it is a frankenstein built from a real HDR title's RPU, not Dolby P9
  tooling output). So unless the base is HDR10 the text report drops the L6 line (`render.rs`;
  with no compat id the profile major decides: P7/P8 default to HDR10, P4/P5 do not) and the HDR
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
  rate and width, a pixel-rate floor only (the bitrate/tier axis is not probed). A declared
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
  the config + RPU the same way, and its **SDR base is inferred from the profile** in
  `hdr::assemble` (P4 is SDR-compatible by definition) since old P4 muxes carry neither a compat
  id nor a base-layer transfer VUI. The **reconstructed bit depth**
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
  recognized (unmatched coordinates suppress the verdict, never guess). The classic trigger is
  re-encode drift (a BT.2020-claiming MDCV over a P3-D65 grade), so the badge is a provenance
  observation, not an error claim. The third sibling is the **`Unconverted RPU` chip**
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
- **Extension dispatch falls back to content sniffing only on error.** `container::demux` picks a
  backend by extension and returns immediately on success — sniffing never runs on the happy path
  (no latency cost). If the extension-matched backend *errors* (e.g. a TS misnamed `.mkv`),
  `sniff_demux` re-probes by magic bytes and is adopted only if a sniffed backend actually
  succeeds; otherwise the original, more specific error is surfaced.
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
  truncation flag into backends**: it drops the TS PCR-span duration and every non-MP4 bitrate
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
  `dependency_pid` names the BL PID it folds into) or that is DV-flagged with no video
  stream_type (the bare EL/RPU PID shape). Such an EL folds into its base layer's track — chunks
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
- **Profile number authority.** libdovi's `dovi_profile` can't express AV1 P10 (returns 5/8),
  so `levels::finalize` takes the profile number from the container dvcC when present, else 10
  for AV1. Don't trust the RPU's profile field for the number.
- **The compat *minor* digit is container-only; a bare RPU can only assume it.** `get_dovi_profile`
  gives the *major* (5/7/8) from the RPU header, but the minor is `dv_bl_signal_compatibility_id`
  (the base-layer type: 8.1 HDR10 vs 8.4 HLG), which lives only in the dvcC/dvvC — the RPU can't
  distinguish them. A metadata-only sidecar has no dvcC: a **DV XML declares its profile**
  (`dv_xml.rs` maps `GenerateProfile` -> compat via `DvAggregate::set_compat_id`, so the minor is
  real), but a **raw RPU bin has nothing**, so its minor is a convention default (P8 -> .1,
  P7 -> .6, P4 -> .2) recorded as `model::profile_compat_assumed`. That JSON pair is the whole
  story for a sidecar: the **text report drops the Profile line for metadata-only sidecars
  entirely** (`render.rs`) — an RPU is profile-agnostic (dovi_tool's blanket "8" for extracted
  RPUs is remux convention, not a definition) and a DV XML's `GenerateProfile` is an authoring
  target, so a rendered profile reads as a fact the metadata doesn't carry. The P7 default
  also covers the common *video* case of an untouched BDMV M2TS, which has **no `0xB0` DV
  descriptor at all** — Blu-ray signals DV via the HDMV registration descriptor and the playlist
  STN table; only remuxes (tsMuxeR etc.) add the descriptor. That flag is gated to metadata-only
  sidecars via `DvAggregate::mark_metadata_only`: a video input — **even a raw HEVC/AV1 elementary
  stream with no dvcC** — has a base-layer VUI that officially backs the inference, so it's never
  flagged. Don't widen the flag to `cfg.is_none()`; raw bitstreams share that state but aren't
  metadata-only. **A bare Profile 5/10's compat digit is display-completed; the JSON stays
  verbatim.** A raw elementary stream has no dvcC/dvvC to declare the minor, and neither profile
  has a `dv_profile_label` convention default, so `render.rs::dv_profile_display` completes the
  digit for the text report and `-q` line when it is certain: a bare "5" ⇒ "5.0" definitionally
  (compat 0 is the only value the P&L spec admits for P5); a bare "10" (P10's compat set is
  {0,1,2,4}) from the base layer's signalled CICP only when the deduction is airtight
  (`infer_p10_compat`): IPT-PQ-c2 matrix ⇒ .0, explicit SDR gamma transfer ⇒ .2, PQ/HLG ⇒ .1/.4
  but only over BT.2020 primaries *and* an explicit non-IPT matrix — IPT is itself PQ-encoded and
  its convention (from P5) leaves CICP unspecified, so PQ alone can't exclude a 10.0 base;
  anything less explicit stays a bare "10". Display only: the JSON
  `profile`/`bl_compatibility_id`/`compatibility` keep the mux's declaration (the bare number /
  null) so machine consumers get raw facts and draw their own inferences — never move this into
  `levels.rs`/`model.rs`.
- **AVC (Profile 9) RPU is found by *content*, not by NAL number.** The DV RPU rides in an H.264
  *unspecified* NAL (Dolby uses type 28; the range is 24..=31), payload = the RPU EBSP beginning
  with the `rpu_nal_prefix` byte `0x19`. `sample.rs` treats an unspecified-range NAL as an RPU only
  when `payload[1] == 0x19` **and** libdovi validates it (CRC): so an atypical mux using another
  unspecified type still parses, and a non-DV unspecified NAL is never misread. libdovi has no
  AVC entry point, but its parsing is codec-agnostic once the header is off — `dv::rpu::parse_avc_rpu`
  strips the **1-byte** AVC header, clears emulation prevention (`bits::ebsp_to_rbsp`), and calls
  `DoviRpu::parse_rpu` (which locates the `0x19` prefix). Don't route AVC through
  `parse_unspec62_nalu` — that strips a **2-byte** HEVC header. **Codec authority:** MP4 from the
  sample entry (`avc1`/`avc3`/`dva1`/`dvav` → `Codec::Avc`), MKV from the `V_MPEG4/ISO/AVC` CodecID
  (CodecPrivate is an `avcC`; `parse_avcc_record`'s embedded SPS supplies depth/chroma/profile —
  also what gives an SDR AVC MKV its 8-bit / Hi10P 10-bit report), TS from PMT `stream_type`
  (`0x1B` AVC vs
  `0x24` HEVC), falling back to DV profile 9 ⇒ AVC only when no video `stream_type` is present (a
  bare DV/EL PID). P9 has no EL and an SDR base (CCID 2 ⇒ `SDR (fallback)` in `hdr::assemble`, the
  same branch Profile 4 uses); its Rec.709 VUI (`0,1,1,1,0`) collapses to a single `BT.709` label
  because primaries == transfer (unlike P5, whose encoding differs from its colour space).
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
  counterpart is the `Frontier` below), under `--no-rpu` (no chunk is read), and for TS
  (chunks index into `reassembled`, not the file). **`--full` on a strict-remote volume
  tailgates `prefetch::Frontier`**, a bounded look-ahead warm riding the progress-tick sites:
  each whole-file walk calls `ensure(pos)`/`ensure_to(end)` so the file crosses the wire once,
  linearly, instead of thousands of scattered fault round-trips. The bytes land in the OS page
  cache only (owned heap unchanged), the look-ahead is capped (`FRONTIER_AHEAD`, with exact
  known spans — an MKV cluster, a scan batch, a TS window — warmed whole since they're consumed
  immediately), and the frontier is monotonic per file. Every container is single-pass under
  `--full` (fused or moov-indexed) — MKV/TS stream in windows, MP4 scans its moov-indexed
  chunks in file order, and raw HEVC/AV1 fuse their whole-stream walk with extraction in
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
  warmed tail is exactly `ts::TAIL_SCAN_BYTES` for the last-PCR duration read; **MKV without a
  Cluster SeekHead entry** falls back to the old handshake, `prefetch::HEAD_WARM` >= the first
  block's offset + `mkv::HEAD_SPAN_BYTES` (with a resolved cluster the coupling is structural:
  `MKV_HEAD_WARM` holds only the front metadata, and the block span is warmed by exact extent).
  Warm via a positioned `ReadFile`/`read_at`, **not**
  `Mmap::advise` (memmap2's advise is `#[cfg(unix)]`, a no-op on the Windows/SMB target).
- **Malformed-input safety in `mp4.rs`.** `read_u32/u16/u64` are bounds-safe (return 0 on OOB);
  any box-declared count fed to a loop/alloc must go through `clamp_count`. Apply the same
  discipline to new table parsing.
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
  stream count-only so the exact rate survives). Otherwise an *overall* rate (file length ÷
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

Cross-check against `mediainfo --Output=JSON` / `ffprobe` / `dovi_tool info` (the ground truth
used throughout). The corpus lives in `testfiles/integration/` (the whole `testfiles/` tree is
local-only and gitignored — nothing under it is committed). For robustness work, byte-mutation
fuzz the release binary over the corpus and assert no `panicked`/exit codes outside {0,2}.
