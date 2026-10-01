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
inputs into an engine `Plan` through `plan_core` and renders the engine's events; the
work itself (opening a drive, keys, copying, recovery, muxing) belongs to
`freemkv_engine`. `tests/engine_entry_guard.rs` fails on a new direct call to that
work from front-end code. Every line in the `Unreleased` section of `CHANGELOG.md`
names the front ends it covers — `(CLI, app, server)` — or says why it reaches only
some: `(CLI only: an exit code)`. `tests/changelog_coverage.rs` checks it.

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
