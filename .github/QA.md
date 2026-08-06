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

## Release candidates

Every push to `qa` stamps `v<version>-rc<N>`, N incrementing, before the gates
run. That answers "which build is on qa, and is it the one I tested?" without
anyone having to remember. When `main` advances, the plain `v<version>` tag is
what ships, and the rc history says how many candidates it took.

The tag is stamped whether the run goes green or red, deliberately — a red
candidate needs a name more than a green one does. `release.yml` excludes
`v*-rc*`, so a candidate never publishes a release.

## Every script that used to be run by hand

The point of this gate is that "did I remember to run that?" stops being a
question. Current state of each:

| script | what it proves | now runs |
|---|---|---|
| `scan-secrets.sh` | no leaks in public repos | `leak-guard.yml`, every push |
| `precommit.sh` — fmt, clippy | style + lint on the CI toolchain | `ci.yml`, every push |
| `precommit.sh` — `cargo test` | the unit suite | `ci.yml`, every push |
| `precommit.sh` — `test --release` | release-profile behaviour | `qa.yml`, push to `qa` |
| `precommit.sh` — Linux cross-clippy | cfg-gated code lints on its own target | `qa.yml`, push to `qa` |
| `tests/cli-integration.sh` | the whole CLI file surface, ffprobe-verified | `qa.yml`, push to `qa` |
| `prerelease.sh` phase 1 (static) | secret scan across public repos | `leak-guard.yml` |
| `prerelease.sh` phase 2 (gate) | per-crate fmt/clippy/test | `ci.yml` + `qa.yml` |
| `prerelease.sh` phase 3 (contract) | CLI contract + exit codes | **still manual** |
| `prerelease.sh` phase 4 (media) | real-media rips end to end | **still manual — needs hardware** |
| `cli-acceptance.sh` | real-ISO acceptance, ~96 checks | **still manual — needs hardware** |

`precommit.sh` stays useful as the *local* fast loop — it is the same gate, run
before you push rather than after. It is no longer the thing standing between a
mistake and a release.

The three still-manual rows are the honest remainder. Two of them need physical
media and are covered under "the one leg that still needs hardware" below.

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

## Registering the `freemkv-media` runner

The real-media job runs on a self-hosted runner labelled `freemkv-media`, on a
host with the ISO hoard mounted. The recipe below is the one that was proven by
hand before the job was written — a container with the toolchain, ffmpeg, the
hoard read-only, and the sibling crates patched to their local checkouts.

Two things that are easy to get wrong and were both hit while proving it:

1. **The sibling patch is not optional.** Without a `.cargo/config.toml`
   redirecting the git-tag deps at the checked-out siblings, the build resolves
   the RELEASED libfreemkv and tests code that is not under test. It fails
   loudly (`cannot find value scan_dir`), but a future version might not.
2. **The suite must be able to run on Linux.** It could not: `stat -f %z` (BSD)
   and `stat -c %s` (GNU) are not two spellings of one thing — on Linux
   `stat -f` is *filesystem* status and SUCCEEDS, so a `||` fallback never
   fires. That produced a phantom "size mismatch" on the first Linux run.

The runner needs, in addition to the hoard:

- **`keydb.cfg`**, or every AACS family skips. Set `FMKV_KEYDB` (repo variable).
- **`FMKV_KEY_URL` / `FMKV_KEY_AUTH`** (repo secrets) for the online key
  service. Without them the UHD family skips even WITH a keydb, because a
  keydb holds VUKs only for discs it has already seen — verified: a 62 MB
  keydb had no entry for the UHD fixture.
- **The acceptance suite itself**, provisioned on the runner out of band, with
  its path in the `FMKV_ACCEPTANCE` repo variable. The workflow does NOT clone
  it: this file is public, and naming the repository or host it lives on would
  put internal infrastructure into a public workflow. The leak guard refuses
  that, correctly — it caught exactly this on the first attempt.

An existing runner on another host is registered to a different org with the
hoard not mounted, so it cannot be reused — register a new one scoped to the
`freemkv` org, and do not enable it for forked-PR triggers, since a self-hosted
runner executes repo workflow code on that host.

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
