# Signing and notarizing the macOS builds

Until the secrets below exist, macOS builds are **ad-hoc signed**: they run on
the machine that built them and are refused everywhere else, because an ad-hoc
signature identifies nobody. What a user sees after downloading one is

> "freemkv" Not Opened — Apple could not verify "freemkv" is free of malware…

That dialog is Gatekeeper refusing a quarantined, un-notarized binary. It is not
a build failure, and re-downloading never helps.

Note this applies to the **bare CLI binary** as much as the `.app` — anything
downloaded carries the quarantine attribute.

## What has to exist (only the account holder can create these)

1. **Apple Developer Program membership** — $99/yr. Individual is fine.
   An organization needs a D-U-N-S number and takes longer to approve.

2. **A "Developer ID Application" certificate.** In Xcode: Settings → Accounts →
   Manage Certificates → **+** → Developer ID Application. Then export it from
   Keychain Access as `.p12` with a password.

   It must be *Developer ID Application*. A "Development" or "Apple
   Distribution" certificate looks similar, signs successfully, and is then
   rejected at notarization — so the workflow checks the authority by name and
   fails early rather than at the end of a release.

3. **An App Store Connect API key** for notarization. appstoreconnect.apple.com
   → Users and Access → Integrations → **+**, role *Developer*. Download the
   `.p8` **once** (it cannot be downloaded again) and note the **Key ID** and
   **Issuer ID**.

   Preferred over an Apple ID + app-specific password: it does not break when
   the account password or 2FA changes.

## The repository secrets

Set on `freemkv/freemkv` (Settings → Secrets and variables → Actions):

| Secret | What it is |
| --- | --- |
| `MACOS_CERT_P12` | the `.p12`, base64: `base64 -i cert.p12 \| pbcopy` |
| `MACOS_CERT_PASSWORD` | the password the `.p12` was exported with |
| `MACOS_KEYCHAIN_PASSWORD` | any random string; names the throwaway keychain CI creates |
| `MACOS_NOTARY_KEY_P8` | the `.p8`, base64: `base64 -i AuthKey_XXX.p8 \| pbcopy` |
| `MACOS_NOTARY_KEY_ID` | Key ID, e.g. `ABCD1234EF` |
| `MACOS_NOTARY_ISSUER_ID` | Issuer ID (a UUID) |

Nothing else changes. `release.yml` detects the secrets and signs; with them
absent it stays ad-hoc, which is what lets a fork — which can never read
secrets — keep building.

## What the release then does

* signs the CLI binary, then the `.app`, **inner-out and without `--deep`**.
  `--deep` is deprecated and re-signs nested code with the outer options, which
  is a common way to produce a bundle notarization rejects.
* `--options runtime` (hardened runtime) and `--timestamp` on every signature.
  Notarization refuses anything missing either, with an unhelpful error.
* submits the `.dmg` to `notarytool --wait`, then **staples** the ticket to the
  `.dmg` and the `.app`. Stapling is what makes an offline first launch work;
  without it, a Mac with no network still refuses the app.
* builds the `.zip` and every checksum **after** stapling — a zip made earlier
  would carry an unstapled app, and its hash would not match what ships.
* verifies with `spctl -a -t install` and `stapler validate`. `codesign
  --verify` alone is not enough: it only says the signature is well-formed, so
  an ad-hoc bundle passes it and is still refused on every other Mac.

## Known limit: the bare CLI binaries

A notarization ticket can only be stapled to a `.app`, `.dmg` or `.pkg` — never
to a bare Mach-O. The CLI binaries are signed and notarized, so Gatekeeper
accepts them, but the first launch needs Apple to be reachable for the online
check. The `.dmg` is the artifact that verifies fully offline.

## Verifying a real release

Download the artifact from the release page — do not test the local build, which
has no quarantine attribute and therefore proves nothing:

```sh
spctl -a -vvv -t install freemkv-aarch64-macos.dmg   # accepted / Notarized Developer ID
xcrun stapler validate freemkv-aarch64-macos.dmg     # The validate action worked!
codesign -dv --verbose=4 /Applications/freemkv.app 2>&1 | grep Authority
```

If `spctl` says `rejected`, the app was signed but never notarized. If it says
`accepted` but a user still sees the dialog, the ticket was not stapled.
