# Security Policy

## Supported versions

| Version | Supported |
| ------- | --------- |
| 1.6.x   | Yes       |
| < 1.6   | No        |

Only the current 1.6.x line receives security fixes.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub Security Advisories:
https://github.com/freemkv/freemkv/security/advisories/new

Do not open a public issue for a security report. Include the affected
version, steps to reproduce, and the impact you believe the issue has.

## Response time

You will get an initial response within 7 days. Reporters are credited in the
resulting advisory and CHANGELOG unless they ask not to be.

## Assurance case

A short argument for why freemkv is acceptably secure to use, and the evidence
behind each claim:

- **Claim: freemkv does not expose users to unsafe network content.**
  Its only network calls are an optional update check and an optional keydb
  fetch, both made over TLS via `ureq`/`rustls` (which validates certificates by
  default). The keydb is verified before it is saved. No code is downloaded or
  executed from the network.
- **Claim: released binaries are what the project built.**
  Releases are produced by the CI release pipeline and code-signed (macOS
  Developer ID, hardened runtime) with SHA-256 checksums published alongside, so
  a tampered download fails signature or checksum verification.
- **Claim: the codebase resists common defects.**
  It is written in Rust (memory safety outside a thin, audited platform-FFI
  layer). Every change must pass `cargo fmt --check`, `cargo clippy
  --all-targets -D warnings`, and the test suite in CI; dependencies are scanned
  with `cargo-deny`, and CI includes leak-, identity-, and comment-guard checks.
- **Claim: secrets are not leaked.**
  No credentials are committed; a leak-guard CI workflow scans changes for
  secrets.
- **Known limitation.** The project is single-maintainer today; see
  [GOVERNANCE.md](GOVERNANCE.md) for the continuity plan and
  [ROADMAP.md](ROADMAP.md) for the effort to add a second maintainer and
  two-person review. The keydb URL may be user-configured to a plain-HTTP
  endpoint; prefer an `https://` source.

This assurance case is revisited as the threat model and the code change.
