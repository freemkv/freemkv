<!--
SPDX-FileCopyrightText: 2026 Matthew Jackson & Contributors
SPDX-License-Identifier: MIT
-->

# Governance

freemkv is an open-source project maintained by Matthew Jackson (the
"maintainer") with contributions from the community. This document states how
decisions are made, who is responsible for what, and how continuity is preserved
if the maintainer becomes unavailable.

## Roles and responsibilities

- **Maintainer** — Matthew Jackson ([@MattJackson](https://github.com/MattJackson)).
  Reviews and merges changes, cuts releases, triages issues and security
  reports, and sets technical direction. Has admin access to the
  [github.com/freemkv](https://github.com/freemkv) organization and publish
  rights for releases.
- **Contributors** — anyone who opens an issue or pull request. Contributions
  are accepted under the project's licence (MIT) and the terms in
  [CONTRIBUTING.md](CONTRIBUTING.md), including the Developer Certificate of
  Origin sign-off.

## How decisions are made

Day-to-day changes are decided by the maintainer on the basis of the
[contribution requirements](CONTRIBUTING.md): a change must build, pass the CI
gate (`cargo fmt --check`, `cargo clippy --all-targets -D warnings`,
`cargo test`), and earn its place for a real user. Larger or user-visible
changes should start as an issue so the direction can be discussed before code
is written. Disagreements are resolved by discussion in the issue or pull
request; the maintainer has the final decision while the project is
single-maintainer.

## Changes to this governance

This document is changed by a pull request like any other file, subject to the
same review.

## Continuity and succession

Because the project is currently single-maintainer (see the "bus factor" fix on
the roadmap), the following measures preserve continuity if the maintainer
becomes unavailable:

- **Everything needed to build, test, release, and operate the project lives in
  the repository** and the [github.com/freemkv](https://github.com/freemkv)
  organization — source, CI workflows, the release pipeline, and documentation —
  not on any individual's machine. The build is reproducible from a clean
  checkout (`Cargo.lock` is committed and CI builds with `--locked`).
- **The organization, not a personal account, owns the canonical repositories,**
  so ownership and access can be transferred by the organization owners without
  losing history or the release channel.
- **Adding a second maintainer with commit and release access is an explicit,
  actively sought goal** (see [ROADMAP.md](ROADMAP.md)); it is the single most
  important continuity improvement and is tracked as such.
- **If the maintainer is unreachable for an extended period,** the project may be
  continued by any capable contributor: fork, and request transfer of the
  organization or the `freemkv` package name through the usual GitHub and
  crates/registry processes. Nothing in the build or release process depends on a
  secret held only by one person except the platform code-signing identity, whose
  absence degrades to an unsigned build (the release workflow handles this
  gracefully) rather than blocking a release.
