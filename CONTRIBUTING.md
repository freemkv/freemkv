# Contributing

Thanks for your interest!

- **Report a bug** — open an issue
- **Submit your drive** — run `freemkv info disc:// --share`
- **Fix a bug** — fork, branch, PR
- **Suggest a feature** — open an issue first

## Development

```bash
cargo build
cargo test
```

Before you open a pull request, the same gate CI runs must pass locally:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features gui -- -D warnings
cargo test
cargo test --features gui
cargo clippy --all-targets --features server -- -D warnings
cargo test --features server
```

The CLI build has no features; the desktop app is `--features gui` (on Linux it
needs `libgtk-4-dev` and `libadwaita-1-dev`). The rip server run by the
container image (`freemkv server`, see `docker/`) is `--features server`. CI
checks all three.

Formatting is rustfmt; clippy must be clean with warnings denied. Behavioural
changes should come with a test.

The CLI, the app and the server are three front ends over one engine. Each turns its
inputs into an engine `Plan` (keys through `plan_core`) and renders the engine's events; the
work itself (opening a drive, keys, copying, recovery, muxing) belongs to
`freemkv_engine`. `tests/engine_entry_guard.rs` fails on a new direct call to that
work from front-end code. Every line in the `Unreleased` section of `CHANGELOG.md`
names the front ends it covers — `(CLI, app, server)` — or says why it reaches only
some: `(CLI only: an exit code)`. `tests/changelog_coverage.rs` checks it.

## Reviewing UI screenshots

CI's `gui-gate` job runs a debug build of the Linux and Windows apps over a synthetic disc,
in English, German and Arabic (right to left) at two scales. After its checks it visits every
screen and state and captures each one: the ready screen with fake drives, the disc page
with each chooser open, the review banner, every menu, About, Settings, the file pickers, a
rip in progress and each result. The tour lives in `src/linux/gate_tour.rs` and in
`review_tour` in `src/windows.rs`, with the shared model states in `src/gui_gate.rs`. A
capture it can't take becomes a `GATE NOTE` line in that run's `gate.txt`, and the build
still passes.

On every push to `dev` (or a manual run of CI), the `ui-gallery` job publishes the captures
to the `ui-screenshots` branch as `<platform>/<lang>-<scale>/<name>.png`. It compares each
one with the approved set in `baseline/` and writes the gallery:

<https://github.com/freemkv/freemkv/blob/ui-screenshots/index.md>

The gallery lists the changed screens first, each with the baseline, the new capture and a
highlight (red: changed pixels; yellow: anti-aliasing, ignored; magenta: area only one
capture has). Then it lists new and missing screens, then every screen. A changed screen
never fails CI: it shows as a warning on the run and in the `ui-gallery` job summary.

To sign off:

1. Open the gallery and check the commit at the top is the one you mean to approve.
2. Review the changed screens. If one is wrong, fix the app and push. The next run
   replaces the gallery.
3. When it is right, run the **ui-approve** workflow (Actions → ui-approve → Run workflow),
   with that commit's SHA, or a prefix of it, as `sha`. It copies the gallery's run into
   `baseline/`, and later runs are diffed against it. If the gallery has moved on to a
   newer run, the approval is refused.

The branch is a single bot commit that holds only the latest run and the baseline, so it
never grows a history. The diff tool is `tools/ui-review` (`cargo test --manifest-path
tools/ui-review/Cargo.toml`). Its tolerances, `--threshold` (0.1) and `--min-pixels` (24),
are set so font-rendering noise passes and a one-word change does not. macOS isn't part of
the gate yet.

## Developer Certificate of Origin (DCO)

Contributions to freemkv are accepted under the
[Developer Certificate of Origin](https://developercertificate.org/) 1.1. By
signing off on your commits you certify that you wrote the change (or otherwise
have the right to submit it) under the project's MIT licence.

Sign off every commit by adding a `Signed-off-by` line with your real name and
email — `git commit -s` does this for you:

```
Signed-off-by: Your Name <your.email@example.com>
```

Pull requests whose commits are not signed off will be asked to amend before
merge.

## Governance

How decisions are made, who is responsible, and how the project continues if the
maintainer is unavailable is documented in [GOVERNANCE.md](GOVERNANCE.md). The
direction is sketched in [ROADMAP.md](ROADMAP.md).

## License

MIT

## Release notes

Describe customer impact, required actions and limitations in public release
notes. Keep build machinery, CI, test counts, refactors and dependency revisions
in commits or internal documentation. Unreleased work stays out of stable notes.
