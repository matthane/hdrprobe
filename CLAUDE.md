# CLAUDE.md — working notes for hdrprobe

Fast HDR / Dolby Vision metadata inspector: one native Rust binary that memory-maps a video
file, demuxes without decoding, samples RPUs, and prints a sectioned report in well under 2s.
**`PLAN.md` is the source of truth** for design, milestones (M1–M8, all complete), and a
detailed per-milestone implementation log — read it before non-trivial changes.

## Commands

```sh
cargo build --release          # binary at target/release/hdrprobe
cargo test                     # 37 unit tests
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

## Module map (`src/`)

- `main.rs` — clap CLI, per-file dispatch, exit codes (0 ok / 1 usage / 2 unreadable).
- `container/` — one hand-rolled demuxer per format: `mp4.rs`, `mkv.rs`, `ts.rs`, `annexb.rs`,
  `av1.rs`; `mod.rs` holds `Demux`/`Chunk`/`DvConfig` and the shared dvcC/hvcC/CICP decoders.
- `hevc/` — `nal.rs` (Annex-B + length-prefixed NAL split), `sps.rs` (dims + VUI colour + VUI
  timing/frame rate).
- `av1/` — `obu.rs` (OBU walker, T.35 routing), `seq.rs` (sequence header).
- `dv/` — `rpu.rs` (libdovi wrapper + panic guard), `levels.rs` (title-stable aggregation).
- `hdr/` — `mod.rs` (format classification), `sei.rs` (ST.2086/CLL/HDR10+/alt-transfer).
- `sample.rs` (parallel sampling), `model.rs` (serde report tree), `render.rs`, `bits.rs`.
- `prefetch.rs` — warms the metadata region with one pipelined positioned read before the
  mmap parse, so SMB/NFS scans don't fault it in over hundreds of round-trips. Timing only —
  parsing still runs against the mmap; gated to remote volumes on Windows (`GetDriveTypeW`).

## Invariants that are easy to violate

- **Zero-copy mmap `Chunk` model.** A `Chunk { offset, size }` is a byte range into the mmap;
  payloads are never copied up front. **Every container backend is hand-rolled on purpose** —
  do *not* add `matroska-demuxer`/`mp4`/etc.; they copy frame data and hide byte offsets, which
  breaks this model. The **one exception is TS/M2TS**, which scatters the elementary stream
  across packets: it fills `Demux::reassembled: Option<Vec<u8>>` and `chunks` index into *that*.
  `sample.rs` picks the source via `reassembled.as_deref().unwrap_or(mmap)`. All other backends
  leave it `None`.
- **Third-party parsers can panic, not just `Err`.** libdovi and the `hdr10plus` crate abort on
  some malformed input. Route *every* call into them through `dv::rpu::guard` (`catch_unwind`).
  **Never re-add `panic = "abort"` to the release profile** — it turns the guard into a no-op.
- **Report title-stable DV levels only.** Show profile/level/compat, L254 (CM version), L6, L9,
  L11, and the *set* of L2/L8 trim targets. Never emit L1 or per-shot trim *values*. **L5 is the
  deliberate exception**: it varies with aspect changes, so it's sampled and shown as the set of
  distinct active areas, labelled `[sampled]` (vs `[full scan]` under `--full`).
- **DV facts and their sources.** BL **compatibility id** and DV **level** come from the
  `dvcC`/`dvvC` box, *not* the RPU. Everything dynamic (FEL/MEL, L5/L6/L9/L11/L254, trim
  targets) comes from the **RPU**, which rides the base layer / a DV-flagged track — the
  enhancement-layer *residual* is decode-only and never needed. This is why P7 dual-track
  "just works" once the BL/DV track's RPU is parsed.
- **Layer/track structure is observed, not assumed per-container.** The report's `Structure` line
  (`Single track, dual layer` vs `Dual track, dual layer`) is rendered only for dual-layer content
  (an EL is present, i.e. Profile 7) and is driven by `Demux::dv_dual_track`, which each backend
  sets from what it actually saw: MP4 from its video-`trak` count (`tracks.len() > 1`), TS/M2TS from
  its video-PID count (a P7 EL rides its own PID), MKV/raw-HEVC/AV1 always `false` (BL+EL interleaved
  in one track, or single-layer). So in practice MKV is always single-track, TS/M2TS always dual, and
  MP4 either — but the label follows the bytes, so an atypical mux is reported correctly rather than
  by rule. `levels::{finalize,container_only}` gate it behind `el_present` via `structure_str`.
- **Profile number authority.** libdovi's `dovi_profile` can't express AV1 P10 (returns 5/8),
  so `levels::finalize` takes the profile number from the container dvcC when present, else 10
  for AV1. Don't trust the RPU's profile field for the number.
- **`--full` changes demux behaviour, not just sampling.** It threads into `container::demux(..,
  full)`: TS reassembles the whole stream (vs a single head window of `ts::HEAD_SCAN_BYTES`), raw
  HEVC scans every byte (vs windowed), MKV indexes every cluster (vs a head byte-window of
  `HEAD_SPAN_BYTES` — without this, walking every block header page-faults the whole multi-GB movie
  off disk), and **raw AV1** walks the whole stream (vs a single head window of `av1::HEAD_SCAN_BYTES`,
  8 MiB). Keep new backends consistent — bounded by default, exhaustive under `--full`. A backend
  that bounds its `chunks` index must not derive fps/frame-count from `chunks.len()` in the bounded
  path. **Raw AV1 is head-window-only, never spread** (unlike raw HEVC's windows): low-overhead OBU
  has no byte-scannable sync marker — AV1 has no emulation prevention, so a temporal-delimiter byte
  pattern can occur inside frame payload — so the demux can only resync from the byte-0 boundary. It
  walks one head window from there, the same head-only shape TS uses; L5 is sampled from it. **Frame
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
- **NAS speed rides on warm-window couplings the corpus can't check.** `prefetch::warm_metadata`
  streams the metadata region in one pipelined read so SMB/NFS scans don't fault it in over
  hundreds of round-trips. Three per-backend couplings, each of which must hold or the demux walks
  past the warmed bytes and faults them one-by-one again: **MKV** — `prefetch::HEAD_WARM` >= the
  first block's offset + `mkv::HEAD_SPAN_BYTES`; **raw AV1** — `av1::HEAD_SCAN_BYTES` (the bounded
  head walk for both OBU and IVF) <= `prefetch::HEAD_WARM`, so the generic head warm covers the whole
  walked span; **TS/M2TS** — the warmed head (chosen by
  `looks_like_ts`) is exactly `ts::HEAD_SCAN_BYTES`, and the demux's packet budget is sized to stay
  within it (`HEAD_SCAN_BYTES / 192`, the larger stride, so the byte span read never exceeds the
  warm for either stride). TS also warms a **trailing** `ts::TAIL_SCAN_BYTES` window for the
  last-PCR duration read (skipped when it overlaps the head on small files) — grow the tail scan
  without growing the warmed tail and the last-PCR read faults in one-by-one again. Shrinking
  `HEAD_WARM`/`HEAD_SCAN_BYTES`, growing `HEAD_SPAN_BYTES` or the
  TS packet budget, or unbounding either index breaks this **silently** — it's timing-only, so tests
  pass and `-q` is unchanged; the regression only shows on a real network path. Warm via a
  positioned `ReadFile`/`read_at`, **not** `Mmap::advise` (memmap2's advise is `#[cfg(unix)]`, a
  no-op on the Windows/SMB target).
- **Malformed-input safety in `mp4.rs`.** `read_u32/u16/u64` are bounds-safe (return 0 on OOB);
  any box-declared count fed to a loop/alloc must go through `clamp_count`. Apply the same
  discipline to new table parsing.
- `split_annexb` treats the buffer start as an implicit NAL boundary (chunks begin at a NAL
  header, not a start code) — relied upon by the length-prefixed and windowed paths.
- **Average bitrate is per-backend and correct-or-labelled, never a wrong number.** Each backend
  fills `Demux::bitrate: Option<Bitrate>` (`model::Bitrate::{video_stream_bps,video_stream,overall}`)
  so container quirks stay local. A *video-stream* rate is emitted only from an exact source: MP4
  sums the `stsz` sizes (exact, free — sample tables, never sample data); MKV prefers the mkvmerge
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
