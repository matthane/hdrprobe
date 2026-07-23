---
description: Generate logical commits and push to the active development branch (never main)
---

Generate logical commits and push to remote. This repo develops each version cycle
on a dedicated branch; main only receives merges at release time via /release, so
work-in-progress (README or schema updates written ahead of the release, etc.)
never bleeds into main.

Perform the following:

- If currently on main, do not commit there. Switch to the active development
  branch for the upcoming version, creating it if it does not exist yet
  (convention: `dev/vX.Y.Z`, e.g. `dev/v0.9.0`, named for the version in
  Cargo.toml's next planned bump). If the upcoming version is unclear, ask with
  AskUserQuestion.
- Group changes into logical commits.
- Write clear and concise messages that accurately describe the changes, matching
  the repo's conventional-commit style (`feat(scope):`, `fix(scope):`,
  `docs(scope):`, `chore(scope):`).
- Include Claude as commit contributor using
  `git commit --trailer "Co-authored-by: Claude <noreply@anthropic.com>"`.
- Push to the development branch only (`git push -u origin <branch>` on first
  push). Never push to main from this skill; main moves only when /release merges
  the cycle in.
