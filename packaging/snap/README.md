# Snap packaging (maintainers)

The recipe is [`snap/snapcraft.yaml`](../../snap/snapcraft.yaml). CI is
[`.github/workflows/snap.yml`](../../.github/workflows/snap.yml).

## What CI does

| Trigger | Build + install + smoke test | GitHub release asset | Snap Store |
|---------|------------------------------|----------------------|------------|
| PR or manual run | yes | — | never |
| `dev` push | yes | — | `edge`, only if the repo variable `SNAP_PUBLISH_EDGE` is `true` |
| `qa` push | yes | — | `beta`, after `CI` and `qa` are green on the same commit |
| `v*` release tag | yes | `freemkv-amd64.snap` + `.sha256` | `stable` |

- The version is `Cargo.toml`'s, plus `+git.<sha>` (dev and PRs) or
  `+qa.<sha>` (qa). A suffix makes the grade `devel`; release tags have none
  and build as `stable`.
- The GitHub release asset is attached whether or not the store is set up.
- The store upload is skipped while `SNAPCRAFT_STORE_CREDENTIALS` is unset.
  With it set, [`store_checks.py`](store_checks.py) judges each upload: an
  upload held for review of exactly the two grants in step 4 (or held with
  no errors) is a warning and a run-summary line saying it was NOT released;
  any other finding, an unrecognised snapcraft output, or an arm64 build that
  did not succeed (failed or cancelled) fails the job. If the store words the
  two grants differently from review-tools, the first upload fails visibly
  and `STORE_GRANT_MARKERS` needs updating.
- Snapcraft is pinned to the `9.x/stable` track (`SNAPCRAFT_CHANNEL`).
  review-tools only has a `latest` track, so the run records its version in
  the summary and `store_checks.py` fails on an unfamiliar report format.
- The release asset needs only the build and the install/CLI checks
  (`smoke`). The GUI launch and review-tools (`review`) gate publishing only,
  so a Snap Store outage cannot block the release asset. The release
  orchestrator treats the snap as optional: a missing one is a warning.
- `dev`, `qa` and PR builds link the matching sibling branches; release tags
  build from the committed git-tag patches.
- arm64 builds on Launchpad (`snapcraft remote-build`) on `qa`, tags and
  manual runs, only when `LAUNCHPAD_CREDENTIALS` is set. It is not smoke
  tested, because no arm64 runner installs it. The build uploads the source
  tree to Launchpad publicly.

## One-time setup

1. Register the name (needs an Ubuntu One account):

   ```bash
   sudo snap install snapcraft --classic
   snapcraft login
   snapcraft register freemkv
   ```

2. Export store credentials limited to this snap and its channels:

   ```bash
   snapcraft export-login --snaps=freemkv \
     --channels=edge,beta,candidate,stable \
     --acls=package_access,package_push,package_update,package_release \
     creds.txt
   ```

3. Add the contents of `creds.txt` as the repository secret
   `SNAPCRAFT_STORE_CREDENTIALS`, then delete the file:

   ```bash
   gh secret set SNAPCRAFT_STORE_CREDENTIALS -R freemkv/freemkv < creds.txt
   rm creds.txt
   ```

   The credentials expire (one year by default); export new ones before then.

4. Ask for store review of the drive interfaces on the
   [Snapcraft forum](https://forum.snapcraft.io/c/store-requests/19): a
   "store-requests" post asking for **allow-connection and auto-connection**
   of `optical-drive` with `write: true` (the `optical-write` plug) for
   `freemkv`. snapd's base declaration denies both for `write: true`, so
   without allow-connection a store install cannot connect it even by hand.
   Explain that libfreemkv opens `/dev/sg*`/`/dev/sr*` read-write for SCSI
   commands (SG_IO), so even reading a disc needs it, and that it locks and
   ejects the tray. Precedent: <https://forum.snapcraft.io/t/write-access-to-optical-drive/8289>.
   Also ask to allow the session `dbus` slot `freemkv-dbus`, which lets the
   app own its GTK application id `org.freemkv.FreeMKV`; review-tools flags it
   and `optical-write` as needing human review, and nothing else.
   In the same post, request auto-connection of `hardware-observe` and
   `removable-media`, and the auto-alias `freemkv-cli` for the
   `freemkv.freemkv-cli` app. Until granted, store uploads wait for manual
   review, and only the direct download (`--dangerous`, connected by hand, see
   [INSTALL.md](../../INSTALL.md)) is usable.

5. Optional: set the repository variable `SNAP_PUBLISH_EDGE` to `true` to
   publish every `dev` push to `edge`:

   ```bash
   gh variable set SNAP_PUBLISH_EDGE -R freemkv/freemkv --body true
   ```

6. Optional, arm64: log in to Launchpad once with
   `snapcraft remote-build` on a machine, then store the
   `launchpad-credentials` file it saves in its data directory
   (`$XDG_DATA_HOME/snapcraft/`) as the secret `LAUNCHPAD_CREDENTIALS`.

## Building locally

`snapcraft pack` from the repository root (needs LXD or Multipass). Without
`snap/local/version-suffix` the version is `<Cargo version>+dev`. A local
`.cargo/config.toml` that points at `../<sibling>` paths does not resolve
inside the build container; move it aside, or create `siblings/<repo>`
checkouts and point the patches there as `snap.yml` does.
