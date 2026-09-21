# OpenSSF Best Practices — passing badge checklist (project 14740)

The engineering side is done and committed to `dev` (badge, SECURITY.md with
private reporting + assurance case, GOVERNANCE.md, ROADMAP.md, CONTRIBUTING.md
with DCO, REUSE.toml/LICENSES). The badge is registered to **github.com/freemkv/freemkv**.

The only remaining work to flip the badge to **passing** is manual form entry on
bestpractices.dev — no code changes needed. Steps for the maintainer:

## Do these (≈15 min) to reach "passing"

1. Log in to https://www.bestpractices.dev with GitHub (as the freemkv org owner)
   and open **project 14740**.

2. **Reporting → bug-report archive URL** (`report_archive`): enter
   `https://github.com/freemkv/freemkv/issues`

3. **Reporting → private vulnerability reporting** (`vulnerability_report_private`):
   enter `https://github.com/freemkv/freemkv/security/advisories/new`
   (or link the repo's SECURITY.md, which documents the same GHSA channel).

4. **Analysis → dynamic analysis** (`dynamic_analysis`, SUGGESTED, non-blocking):
   mark **N/A** with the justification:
   "Primarily memory-safe Rust; the unsafe surface is a thin, audited
   platform-FFI layer (see SECURITY.md)."

5. Skim the auto-detected fields. Once the two MUST reporting URLs (2 and 3) are
   present, the badge flips to **passing**.

## Crypto/security answers — use this honest framing (DRM-adjacent project)

- `crypto_published`: MET — uses published, expert-reviewed crypto (AES via rustls); no home-grown ciphers.
- `crypto_working` (avoid broken algorithms "unless necessary for interoperability"):
  CSS (DVD) is weak by design; freemkv handles it **solely for format interoperability** — state exactly that (the criterion's built-in exception).
- `crypto_random` / `crypto_keylength` / `crypto_password_storage`: N/A — freemkv neither generates keys nor stores passwords; user supplies VUKs via keydb.cfg.
- `no_leaked_credentials`: MET — ships NO AACS/CSS key material (user-supplied); CI leak-guard enforces it.
- Do NOT claim a formal external security audit (that's a gold-level item). The SECURITY.md assurance case is honest as written — leave it as an assurance case.

## Before/at release (so the criteria see the committed docs)

6. Push the `dev` branches when going to QA/release:
   - freemkv `dev` (commit `fe1875f` — OpenSSF docs) and (`d23029f4` — --share diagnostics)
   - freemkv-firmware `dev` (commit `e5e3fed` — badge)

## Optional (free, moves toward silver/gold)

7. Enable org-wide **2FA** for all committers (a gold criterion, one org setting).
