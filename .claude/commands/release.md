---
description: Prepare and cut an hdrprobe release (gates, versioning, schema, notes, tag)
---

Prepare a new hdrprobe release. Execute the steps in this order; the ordering is
load-bearing (the license manifest embeds the crate version, so it must be
regenerated after the version bump, never before).

## 1. Review what is being released

- Find the last version tag (`git describe --tags --abbrev=0`) and read every commit
  since it. Compare against the published notes of the previous GitHub release to
  confirm nothing already shipped is re-announced.
- Ensure documentation reflects the changes: README.md (user-facing only; keep it
  non-technical, no JSON field names or internal mechanics, and credit any newly
  supported format's trademark holders as done for SL-HDR / HDR Vivid), CLAUDE.md
  (design reference), docs/SCHEMA.md, and docs/INTEGRATION-STDIN.md if stdin
  behavior changed.

## 2. Schema version audit

- Check whether anything since the last tag changed `src/model.rs` (fields, presence
  conditions) or a rendered label value space (container/codec/profile/format
  strings, enumerated names). If so, `model::SCHEMA_VERSION` must already carry the
  bump (minor for additive, major for breaking) with a matching docs/SCHEMA.md
  version-history entry; if it was missed, add it now.
- At most one schema bump per release: if several changes since the last tag each
  bumped the version, collapse them into a single bump and a single history entry.
- The new version's history entry ends with "Ships in hdrprobe X.Y.Z." using the
  release version chosen below (fill it in after step 3).
- The unit test `schema_doc_header_matches_schema_version` pins the SCHEMA.md
  header and history entry to `SCHEMA_VERSION`; it failing means the doc and
  constant moved apart.

## 3. Choose the version

Ask the user for the new version number with AskUserQuestion, offering the computed
major, minor, and patch increments from the current Cargo.toml version.

## 4. Update version metadata

- Bump `version` in Cargo.toml, then run a build so Cargo.lock picks it up.
- Sync the README masthead banner: the ASCII-art block hardcodes `vX.Y.Z` and does
  not update itself.
- Fill in the "Ships in hdrprobe X.Y.Z." suffix on the new SCHEMA.md history entry
  (only when this release ships a new schema version).

## 5. Regenerate the license manifest (after the bump, always)

`THIRD-PARTY-LICENSES.md` embeds the crate version in its Used-by lines, so it must
be regenerated after Cargo.toml changes or the tag CI's drift gate fails:

```sh
cargo about generate about.hbs -o THIRD-PARTY-LICENSES.md
git diff --exit-code THIRD-PARTY-LICENSES.md   # nonzero => commit the update
```

Never hand-edit the manifest. If generation fails on an unaccepted license, stop
and resolve it (vet MIT-compatibility before extending `about.toml`'s allowlist);
do not release around it.

## 6. Run every gate locally before tagging

All of these must pass; the tag CI re-runs the first three under
`RUSTFLAGS=-Dwarnings` and a failed tag run is avoidable noise.

- `cargo build --release` with zero warnings.
- `cargo clippy --release --all-targets` with zero warnings (CI's exact
  invocation; `--all-targets` means test code counts too).
- `cargo clippy --release --target x86_64-unknown-linux-gnu`: the Linux gate.
  Helpers whose only callers sit behind `cfg(windows)` are dead code here and
  surface only on this target.
- `cargo test`: all tests pass. Tests must stay path-portable (no Windows path
  literals); the release gate runs them on Linux, macOS, and FreeBSD.
- Corpus byte-identity: `./target/release/hdrprobe testfiles/integration/ -q`
  output unchanged versus the previous release binary, unless a change
  intentionally altered it (then verify each diff line is the intended one).
  `testfiles/` is local-only and gitignored, so CI cannot run this; it is a
  manual pre-tag step and this is the moment for it.

## 7. Draft release notes

Format is always three sections: **New**, **Fixed**, **Schema**.

- New: features highlighted and explained concisely, from the user's perspective.
- Fixed: only bugs that existed in the previous release tag. A bug introduced and
  fixed between tags is development churn and never appears.
- Schema: mandatory even when unchanged (machine consumers check it). Either
  "Unchanged at N.M" or the new version with a one-line summary of what was added
  or broken, matching the SCHEMA.md history entry.
- Mention behavior changes only when they affect usage (CLI arguments, output,
  tooling integration).

## 8. Commit, push, tag

- Commit the release changes (version bump, manifest, docs). Ask for explicit
  approval with AskUserQuestion before pushing to main.
- Tag as `vX.Y.Z` matching Cargo.toml exactly; CI hard-fails on any mismatch.
- Pushing the tag triggers `.github/workflows/release.yml`: it re-runs the gates,
  builds and tests all seven platform targets (Windows x86_64, Linux x86_64 +
  aarch64 glibc + aarch64 static musl, macOS arm64 + Intel via Rosetta, FreeBSD
  x86_64 in a VM), and attaches the archives plus SHA256SUMS to a **draft** GitHub
  release. Nothing publishes automatically.
- Watch the workflow to completion, then paste the release notes into the draft
  release for the user to review and publish manually.
- To exercise the pipeline without cutting a release, use the workflow_dispatch
  trigger instead of a tag: it runs the gates and builds but skips the release job.
