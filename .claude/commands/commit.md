---
description: Generate logical commits and push to the active development branch (never main)
---

Generate logical commits and push to remote. This repo develops every version
cycle on one long-lived `dev` branch; main only receives merges at release time
via /release, so work-in-progress (README or schema updates written ahead of the
release, etc.) never bleeds into main.

Perform the following:

- If currently on main, do not commit there. Switch to `dev`, creating it off
  main if it does not exist yet. The branch is deliberately version-agnostic and
  survives releases, so there is no per-cycle branch to name or guess at.
- Group changes into logical commits.
- Write clear and concise messages that accurately describe the changes, matching
  the repo's conventional-commit style (`feat(scope):`, `fix(scope):`,
  `docs(scope):`, `chore(scope):`).
- Include Claude as commit contributor using
  `git commit --trailer "Co-authored-by: Claude <noreply@anthropic.com>"`.
- Push to `dev` only (`git push -u origin dev` on first push). Never push to main
  from this skill; main moves only when /release merges the cycle in.
