# CLAUDE.md — working notes for hdrprobe

Fast HDR / Dolby Vision metadata inspector: one native Rust binary that memory-maps a video
file, demuxes without decoding, samples RPUs, and prints a sectioned report in well under 2s.
It also parses metadata **sidecar** files (raw DV RPU, DV CM XML, HDR10+ JSON) into the same
report. This file plus the module-level doc comments are the design reference — read the
relevant section and the code it points at before non-trivial changes.

## Commands

```sh
cargo build --release          # binary at target/release/hdrprobe
cargo test                     # 87 unit tests
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
vendor kernels through current), and macOS arm64 + Intel (Intel is
cross-compiled on the arm64 runner and tested via Rosetta), and attaches the archives plus
`SHA256SUMS` to a **draft** GitHub release for manual review. A `workflow_dispatch` run exercises
the gates and builds without creating a release. The corpus `-q` check stays a manual pre-tag step
(`testfiles/` is local-only). The code is deliberately portable outside `shell.rs`/`prefetch.rs`'s
`cfg(windows)` branches — keep new platform-specific code behind `cfg` with a non-Windows path, and
never parse bytes native-endian.

- `main.rs` — clap CLI, per-file dispatch (sidecar files first, then the video pipeline), exit
  codes (0 ok / 1 usage / 2 unreadable).
- `container/` — one hand-rolled demuxer per format: `mp4.rs`, `mkv.rs`, `ts.rs`, `annexb.rs`,
  `av1.rs`; `mod.rs` holds `Demux`/`Chunk`/`DvConfig` and the shared dvcC/hvcC/CICP decoders.
- `hevc/` — `nal.rs` (Annex-B + length-prefixed NAL split), `sps.rs` (dims + VUI colour + VUI
  timing/frame rate).
- `avc/` — the H.264 analogue, for Dolby Vision **Profile 9** (`dvav.09`: 8-bit AVC, single-layer,
  SDR-compatible Rec.709 base). `nal.rs` (1-byte NAL header — `nal_type = byte & 0x1F`), `sps.rs`
  (macroblock-based dims + the profile_idc-gated high-profile chroma/depth block + VUI colour + VUI
  timing). Reuses `hevc::sps::VuiColor` so the shared `container::color_from_vui` plumbing is
  unchanged.
- `av1/` — `obu.rs` (OBU walker, T.35 routing), `seq.rs` (sequence header).
- `dv/` — `rpu.rs` (libdovi wrapper + panic guard), `levels.rs` (title-stable aggregation).
- `hdr/` — `mod.rs` (format classification + `primaries_label`, the chromaticity→gamut matcher
  behind the Mastering line's tag), `sei.rs` (ST.2086/CLL/HDR10+/alt-transfer). The AV1
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
  version renders as the General section's `Schema version` line (`model::General::format_version`),
  present only when an input declares one — today only DV XML sidecars. The XML's Level-0 primaries (tagged `[L0]`) are the
  mastering-gamut fallback for a CM v2.9 XML, which has no L9; a recognized L9 wins when present,
  so CM v4.0 output is unchanged.
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

## Invariants that are easy to violate

- **Zero-copy mmap `Chunk` model.** A `Chunk { offset, size }` is a byte range into the mmap;
  payloads are never copied up front. **Every container backend is hand-rolled on purpose** —
  do *not* add `matroska-demuxer`/`mp4`/etc.; they copy frame data and hide byte offsets, which
  breaks this model. The **one exception is TS/M2TS**, which scatters the elementary stream
  across packets: it fills `Demux::reassembled: Option<Vec<u8>>` and `chunks` index into *that*.
  `sample.rs` picks the source via `reassembled.as_deref().unwrap_or(mmap)`. All other backends
  leave it `None`. **Fragmented MP4 (fMP4/CMAF) stays zero-copy too**: its moov `stbl` tables are
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
  **L10 is never in the tag**: it only *defines* the display an L8 trim points at; the trim itself
  is L8.
- **DV facts and their sources.** BL **compatibility id** and DV **level** come from the
  `dvcC`/`dvvC` box, *not* the RPU. The DV Mastering line's **luminance** is the DM header's
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
  id nor a base-layer transfer VUI.
- **FEL brightness expansion is a metadata verdict with hard gates.** The DV Mastering line's
  `(FEL brightness expansion)` badge (`levels::flag_fel_brightness_expansion`) fires only when
  the RPU is **FEL** *and* the grade's `source_max_pq` exceeds the **base layer's own** declared
  mastering max (container MDCV / ST.2086 SEI) by >10% (e.g. 4000-nit grade over a 1000-nit BL).
  Never flag a MEL (its residual is empty, so it can't out-bright the BL no matter what the
  displays say), never compare against the RPU's own L6 values (self-referential), and never flag
  sidecars (no base layer to expand beyond), so `main.rs` is the only caller. This is a metadata
  verdict only: confirming the general case would mean decoding and comparing composed-vs-BL
  pixels, which hdrprobe never does, so a missing badge is not proof of no expansion.
- **Extension dispatch falls back to content sniffing only on error.** `container::demux` picks a
  backend by extension and returns immediately on success — sniffing never runs on the happy path
  (no latency cost). If the extension-matched backend *errors* (e.g. a TS misnamed `.mkv`),
  `sniff_demux` re-probes by magic bytes and is adopted only if a sniffed backend actually
  succeeds; otherwise the original, more specific error is surfaced.
- **Layer/track structure is observed, not assumed per-container.** The report's `Structure` line
  (`Single track, dual layer` vs `Dual track, dual layer`) is rendered only for dual-layer content
  (an EL is present, i.e. Profile 4 or 7) and is driven by `Demux::dv_dual_track`, which each backend
  sets from what it actually saw: MP4 from its video-`trak` count (`tracks.len() > 1`), TS/M2TS from
  its video-PID count (a P7 EL rides its own PID, so >1 video PID), MKV/raw-HEVC/AV1 always `false`
  (BL+EL interleaved in one track, or single-layer). So in practice MKV is always single-track; TS/M2TS
  is dual for Profile 7 (BL and EL on separate PIDs) but single-track for legacy Profile 4, whose EL is
  interleaved into one PID (the corpus `dv4_hevc.ts` reads `Single track, dual layer`); MP4 is either —
  but the label follows the bytes, so an atypical mux is reported correctly rather than by rule.
  `levels::{finalize,container_only}` gate it behind `el_present` via `structure_str`.
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
  metadata-only.
- **AVC (Profile 9) RPU is found by *content*, not by NAL number.** The DV RPU rides in an H.264
  *unspecified* NAL (Dolby uses type 28; the range is 24..=31), payload = the RPU EBSP beginning
  with the `rpu_nal_prefix` byte `0x19`. `sample.rs` treats an unspecified-range NAL as an RPU only
  when `payload[1] == 0x19` **and** libdovi validates it (CRC): so an atypical mux using another
  unspecified type still parses, and a non-DV unspecified NAL is never misread. libdovi has no
  AVC entry point, but its parsing is codec-agnostic once the header is off — `dv::rpu::parse_avc_rpu`
  strips the **1-byte** AVC header, clears emulation prevention (`bits::ebsp_to_rbsp`), and calls
  `DoviRpu::parse_rpu` (which locates the `0x19` prefix). Don't route AVC through
  `parse_unspec62_nalu` — that strips a **2-byte** HEVC header. **Codec authority:** MP4 from the
  sample entry (`avc1`/`avc3`/`dva1`/`dvav` → `Codec::Avc`), TS from PMT `stream_type` (`0x1B` AVC vs
  `0x24` HEVC), falling back to DV profile 9 ⇒ AVC only when no video `stream_type` is present (a
  bare DV/EL PID). P9 has no EL and an SDR base (CCID 2 ⇒ `SDR (fallback)` in `hdr::assemble`, the
  same branch Profile 4 uses); its Rec.709 VUI (`0,1,1,1,0`) collapses to a single `BT.709` label
  because primaries == transfer (unlike P5, whose encoding differs from its colour space).
- **`--full` changes demux behaviour, not just sampling.** It threads into `container::demux(..,
  full)`: TS reassembles the whole stream (vs a single head window of `ts::HEAD_SCAN_BYTES`), raw
  HEVC scans every byte (vs a single head window of `annexb::HEAD_SCAN_BYTES`, 8 MiB, dropping the
  boundary-cut trailing NAL), MKV indexes every cluster (vs a head byte-window of
  `HEAD_SPAN_BYTES` — without this, walking every block header page-faults the whole multi-GB movie
  off disk), and **raw AV1** walks the whole stream (vs a single head window of `av1::HEAD_SCAN_BYTES`,
  8 MiB). Keep new backends consistent — bounded by default, exhaustive under `--full`. A backend
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
  ~one 4K GOP (~10 MiB) in. So `ts::Limits::sampled` is a *single* head window bounded by `HEAD_SCAN_BYTES`
  (24 MiB, ~2× the observed SPS depth) with the AU/byte caps lifted so the read isn't cut short
  before that IDR. Don't "optimize" this down to a few MiB (drops resolution/colour, and L5 falls
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
  whole). The chunk warm is skipped under `--full` (every chunk is read anyway;
  pre-reading a whole movie would regress), under `--no-rpu` (no chunk is read), and for TS
  (chunks index into `reassembled`, not the file). The `sidx`/`mfra` ranges are a **hint
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
  the summed block index *only when complete* (`!stopped_early`); TS sums the reassembled stream
  under `--full`. Otherwise an *overall* rate (file length ÷ duration, labelled distinctly because it
  counts audio + overhead) or `None` (no duration: raw HEVC/AV1). Never divide a bounded head-window
  index by the full runtime. **MKV reads the statistics `Tags` via one bounded tail seek**: mkvmerge
  writes `Tags` after the clusters, past the head window, so the demux follows the front SeekHead's
  Tags pointer (`seekhead_tags_offset`) and parses just that small element (`parse_tags_at`). This is
  the *only* place the MKV default path touches the tail — a single bounded read, warmed on NAS by
  `prefetch` (which resolves the same extent via `mkv::tags_extent` and streams it alongside the head,
  mirroring the TS tail-PCR warm; keep the two in sync). Under `--full` the walk reaches `Tags`
  naturally. A track may carry several `Tag`s for one UID (e.g. SOURCE_ID before the statistics), so
  select the first entry with a usable value, not the first UID match.

## Verifying changes

Cross-check against `mediainfo --Output=JSON` / `ffprobe` / `dovi_tool info` (the ground truth
used throughout). The corpus lives in `testfiles/integration/` (the whole `testfiles/` tree is
local-only and gitignored — nothing under it is committed). For robustness work, byte-mutation
fuzz the release binary over the corpus and assert no `panicked`/exit codes outside {0,2}.
