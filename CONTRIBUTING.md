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
cargo test
```

Formatting is rustfmt; clippy must be clean with warnings denied. Behavioural
changes should come with a test.

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
