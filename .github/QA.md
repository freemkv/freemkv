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
| `prerelease.sh` phase 4 (media) | real-media rips end to end | `qa.yml` `cli-matrix`, push to `qa` |
| `cli-acceptance.sh` | real-ISO acceptance, ~96 checks | `qa.yml` `cli-matrix`, push to `qa` |

`precommit.sh` stays useful as the *local* fast loop — it is the same gate, run
before you push rather than after. It is no longer the thing standing between a
mistake and a release.

One still-manual row is the honest remainder: the CLI contract check. The media
rows are no longer manual — they run on ephemeral EC2 runners against full-size
disc images in S3, described under "the real-media gate" below.

## What runs where, and why

| check | where | trigger |
|---|---|---|
| fmt / clippy / unit tests / leak-guard | GitHub Actions (`ci.yml`) | every push to dev, qa, main + PRs |
| **release-profile tests, 3 platforms** | GitHub Actions (`qa.yml`) | push to `qa` |
| **Linux cross-target clippy** | GitHub Actions (`qa.yml`) | push to `qa` |
| **CLI integration, end to end** | GitHub Actions (`qa.yml`, `freemkv`) | push to `qa` |
| **cross-platform output parity** | GitHub Actions (`parity.yml`) | push to `qa` + manual |
| **real-media acceptance, full-size** | ephemeral EC2 (linux + windows) | push to `qa` — see below |
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

## The real-media gate

`disc://` and real `iso://` need media no hosted runner has: hosted runners
ship ~14 GB of disk against a 34 GB Blu-ray and a 53 GB UHD. So `qa.yml` runs
the acceptance suite on **ephemeral EC2 runners**, one Linux and one Windows,
against full-size disc images held in S3.

Three jobs, in order:

| job | what it does |
|---|---|
| `launch-runners` | mints a single-use registration token per OS and starts one EC2 instance from a launch template |
| `cli-matrix` | builds the CLI, fetches four fixtures, runs `cli-acceptance.sh`, emits a hash ledger |
| `compare-cli-matrix` | fails unless Linux and Windows produced byte-identical output |

**Fixtures — one per family**, in the bucket named by the `FMKV_FIXTURES_BUCKET` secret:
`dvd.iso` (CSS), `bd.iso` (AACS v1), `uhd.iso` (AACS 2.0), `hddvd.iso`
(MPEG-PS/VC-1). ~121 GiB, held for the whole run rather than fetched per rip,
because the format, selection and `dir://` groups revisit them — the launch
templates carry 600 GB for that reason.

**Keys.** `FMKV_KEY_URL` / `FMKV_KEY_AUTH` (repo secrets) point at the online
key service, the same one autorip ships with by default, so CI tests the
configuration users actually have. A local `keydb.cfg` is not enough on its
own: it holds VUKs only for discs it has already seen, and a 62 MB keydb had
no entry for the UHD fixture. The job checks both secrets are set BEFORE
fetching 121 GiB, because otherwise the run gets six minutes in, rips the DVD
fine (CSS needs no key) and only then reports a key error that reads like a
media defect.

**Teardown — three independent mechanisms**, because each can fail alone:
`--ephemeral` de-registers the runner after one job; `shutdown` against
`InstanceInitiatedShutdownBehavior=terminate` deletes the instance and its
volume; and `ci-runner-sweeper.yml` terminates anything tagged `freemkv-ci`
older than 5 h from OUTSIDE the instance — the only one that survives
user-data dying before it arms the other two.

**Cost control is `needs:`, not decoration.** Nothing touches real media until
lint, the unit suites, the cross-lint, the Windows build and the synthetic CLI
matrix are green, so a typo cannot burn a two-hour rip. This is also why the
matrix is `workflow_dispatch` in `ci-runner-launch.yml`: a push trigger is how
a busy afternoon becomes a surprise bill.

### What this gate does NOT cover

**macOS, on full-size media.** EC2 Mac requires a Dedicated Host with a
24-hour minimum allocation, which is not worth it per qa push. macOS is still
compared on hosted runners by `hash-matrix.yml`, against synthetic media and a
35 MB DVD fixture — so the mux and the UDF walk are proven there; what is not
proven on macOS is full-size, multi-family real media.

**Seamlessly branched titles.** The UHD fixture is a single-clip title. The
suite previously defaulted to a branched one specifically to exercise the
multi-clip seam join; swapping `uhd.iso` for a branched disc restores that
coverage without any CI change.

Both are stated here rather than left implicit, because a green `qa` should
mean a known thing. The two most recent shipped defects — multi-clip A/V
drift, and a CSS false-positive that silently destroyed a DVD's title table —
were both invisible to every hardware-free check, which is what this gate
exists for.
