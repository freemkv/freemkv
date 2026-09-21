<!--
SPDX-FileCopyrightText: 2026 Matthew Jackson & Contributors
SPDX-License-Identifier: MIT
-->

# Roadmap

This is a high-level, non-binding view of where freemkv is heading. Dates are
intentionally omitted; priorities shift with what real users need. Concrete work
is tracked in [GitHub issues](https://github.com/freemkv/freemkv/issues).

## Now

- **Windows desktop app** — reach parity with the macOS app (the CLI is already
  cross-platform).
- **Broaden drive support** — grow the hardware profile database from community
  submissions (`freemkv info disc:// --share`).

## Next

- **More translations** — extend beyond the current 29 shipped locales.
- **Project resilience / OpenSSF hardening** — close the remaining OpenSSF Best
  Practices items (see below), including reproducible-build verification and
  higher test coverage.

## Later / help wanted

- **A second maintainer.** The project is currently single-maintainer. Adding at
  least one more maintainer with commit and release access is the most important
  resilience goal — it raises the "bus factor" and unlocks two-person code review.
  This is explicitly community-solicited; see
  [GOVERNANCE.md](GOVERNANCE.md#continuity-and-succession).

## Not planned

See the README for what freemkv deliberately does not do. Scope is kept narrow on
purpose: rip, remux, and transfer between disc, file, and network.
