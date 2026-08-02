# The `qa` gate

`dev` → **`qa`** → `main`.

`dev` runs the fast checks (fmt, clippy, unit tests, leak-guard) on every
push. `qa` is the release-candidate branch: merging into it runs the expensive
suite, and `qa` is therefore always a pointer to "the last thing that passed
everything". `main` only ever receives a `qa` that is green.

## What runs where, and why

| check | where | trigger |
|---|---|---|
| fmt / clippy / unit tests / leak-guard | GitHub Actions | every push |
| **cross-platform output parity** | GitHub Actions (`parity.yml`) | `qa` + manual |
| **GUI automation** | self-hosted or EC2 | `qa` |
| mutation runs (~12k mutants, ~1 h) | EC2, sharded | pre-release only |

Mutation testing is deliberately NOT per-push: an hour and real money per run
buys nothing on a commit that only touches a comment. It is a release gate.

## Two traps the parity job exists to avoid re-learning

1. **`MUX_APP` is written into every MKV.** `libfreemkv/src/mux/mkv.rs` writes
   `MuxingApp`/`WritingApp` = `freemkv <version> (g<sha>)`, and the sha comes
   from **libfreemkv's** `build.rs`. Two runs built from different libfreemkv
   commits produce different output bytes while being functionally identical.
   The job asserts the shas match before it compares hashes, so a red result
   means a real divergence rather than a version stamp.
2. **ffmpeg differs per platform.** Generating the media on each runner bakes
   that runner's encoder into the input. The media is generated once and
   shared as an artifact.

Both produce a red build that looks like a defect and is not one.

## Why the GUI is not in GitHub Actions

GitHub's Windows runners execute as a service account, so a launched window
lands in session 0 — no visible window station, and UI Automation cannot reach
it. A GUI test there would pass while asserting against nothing, which is
worse than no test. macOS runners have the mirror problem: driving AppKit needs
Accessibility permission that a hosted runner does not grant.

So the GUI leg needs a self-hosted runner with a real logged-in desktop, or the
EC2 approach in `freemkv-private/scripts/parity` (autologon + a Startup
shortcut, which is what makes the session interactive).

## What parity does NOT cover

`disc://` and real `iso://` need physical media. No hosted runner has an
optical drive, so those stay on the hardware checklist.
