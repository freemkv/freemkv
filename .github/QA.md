# The `qa` gate

`dev` → **`qa`** → `main`. Every public freemkv repo follows this.

`dev` is for committing often. `ci.yml` runs the fast checks on every push —
fmt, clippy, unit tests, leak-guard — so a mistake surfaces in minutes while it
is still cheap to fix. Push to `dev` freely; that is what it is for.

`qa` is the release candidate. Pushing to it runs `ci.yml` **and** `qa.yml`,
which is everything expensive that can run without physical media. A green `qa`
is the claim "this is production worth", and `qa` is therefore always a pointer
to the last thing that passed everything.

`main` only ever receives a `qa` that is green. `release.sh` advances it to the
new tag as its final step.

When `qa` fails: fix on `dev`, get `dev` green, push to `qa` again. Never patch
`qa` directly — a fix that never passed the fast gate has skipped a step, and
`dev` stops being the branch that reflects what is being released.

## What runs where, and why

| check | where | trigger |
|---|---|---|
| fmt / clippy / unit tests / leak-guard | GitHub Actions (`ci.yml`) | every push to dev, qa, main + PRs |
| **release-profile tests, 3 platforms** | GitHub Actions (`qa.yml`) | push to `qa` |
| **Linux cross-target clippy** | GitHub Actions (`qa.yml`) | push to `qa` |
| **CLI integration, end to end** | GitHub Actions (`qa.yml`, `freemkv`) | push to `qa` |
| **cross-platform output parity** | GitHub Actions (`parity.yml`) | push to `qa` + manual |
| **real-media acceptance** | self-hosted runner | push to `qa` — see below |
| **GUI automation** | self-hosted or EC2 | `qa` |
| mutation runs (~12k mutants, ~1 h) | EC2, sharded | pre-release only |

Mutation testing is deliberately NOT per-push: an hour and real money per run
buys nothing on a commit that only touches a comment. It is a release gate.

## Why `qa.yml` checks siblings out at `qa`, not `dev`

This is not a monorepo. Every job checks out the sibling repos it needs, and
`ci.yml` has always taken them from `dev`. For a `qa` run that is wrong: it
would validate the release candidate against unreleased dev tips, so a green
result would describe a combination that is not the one shipping — the exact
mismatch this branch model exists to prevent.

`qa.yml` pins siblings to `qa`. `ci.yml` tracks whichever branch triggered it
(`github.ref_name == 'qa' && 'qa' || 'dev'`), so it stays correct on both.

**Consequence: the repos must move to `qa` together.** A crate whose siblings
have not reached `qa` yet cannot resolve them. Push in dependency order, the
same order `release.sh` tags in: `freemkv-unlock` → `libfreemkv` →
`freemkv-keysources` → `freemkv-i18n` → `freemkv-engine` → `bdemu` / `freemkv`
/ `autorip`.

## Why release-profile tests are a separate gate

The debug suite runs on every dev push. Release is a *different build*:
overflow checks are off, `debug_assert!` is compiled out, and inlining changes
what the optimiser can prove. A test that only passes in debug never guarded
the binary anyone actually ships.

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

So the GUI leg needs a self-hosted runner with a real logged-in desktop, or an
EC2 instance configured with autologon plus a Startup shortcut — that pairing
is what makes the session interactive, and it is what our conformance harness
uses.

## The one leg that still needs hardware

`disc://` and real `iso://` need physical media: an optical drive for the
former, the image hoard for the latter. No hosted runner has either, so the
real-media acceptance suite runs on a **self-hosted runner** labelled
`freemkv-media`, on a host that has the hoard mounted and a drive attached.

Until such a runner is registered, that job cannot run, and a green `qa` means
"everything that does not need hardware passed" rather than "everything
passed". That distinction is worth stating out loud on any release, because
the two most recent shipped defects — multi-clip A/V drift, and a CSS
false-positive that silently destroyed a DVD's title table — were both
invisible to every hardware-free check and were caught only on real media.
