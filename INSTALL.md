# Installing freemkv

freemkv ships as two builds of the same program, both installed as `freemkv`:

| Build | What you get | `freemkv` with no arguments | `freemkv <args>` |
|-------|--------------|-----------------------------|------------------|
| **App** | Desktop window **and** the full CLI | opens the window | the CLI |
| **CLI** | Command line only, no UI libraries | prints usage (exit 2) | the CLI |

Pick one. **Install only one build at a time**: both put a `freemkv` command
on your `PATH`, and the packages replace each other (the Debian packages and
the Homebrew cask/formula conflict by design).

`freemkv gui` also opens the window in the app build. On Linux, a bare
`freemkv` from the app build prints usage when no display is available
(e.g. over SSH).

Every release is at <https://github.com/freemkv/freemkv/releases/latest>, and
each asset has a matching `<asset>.sha256`. Assets use stable names, so
`https://github.com/freemkv/freemkv/releases/latest/download/<asset>` always
points at the newest release.

## Assets

| Platform | App | CLI |
|----------|-----|-----|
| macOS Apple Silicon | `freemkv-aarch64-macos.dmg` / `.zip` | `freemkv-cli-aarch64-macos` |
| macOS Intel | `freemkv-x86_64-macos.dmg` / `.zip` | `freemkv-cli-x86_64-macos` |
| Windows x86_64 | `freemkv-x86_64-windows-setup.exe` (installer), `freemkv-x86_64-windows.zip` (portable) | `freemkv-cli-x86_64-windows.exe` |
| Linux x86_64 | `freemkv-amd64.deb`, `freemkv-x86_64-linux.AppImage`, `freemkv-x86_64-linux.flatpak` | `freemkv-cli-amd64.deb`, `freemkv-cli-x86_64-linux` |
| Linux arm64 | — | `freemkv-cli-aarch64-linux` |

The Linux CLI binaries are static (musl) and need no shared libraries. The
macOS app and CLI are Developer ID signed and notarized.

The CLI binaries are also published under their pre-split names
(`freemkv-x86_64-linux`, `freemkv-aarch64-linux`, `freemkv-x86_64-macos`,
`freemkv-aarch64-macos`, `freemkv-x86_64-windows.exe`) for one more release, so
existing download scripts keep working. Switch to the `freemkv-cli-*` names.

## macOS

**App.** Open the `.dmg` and drag `freemkv.app` to Applications, or:

```bash
brew install --cask freemkv/tap/freemkv
```

The cask also links the `freemkv` command. With the `.dmg`, link it yourself:

```bash
sudo ln -sf /Applications/freemkv.app/Contents/MacOS/freemkv /usr/local/bin/freemkv
```

**CLI.**

```bash
brew install freemkv/tap/freemkv-cli
```

Or download the binary directly (Apple Silicon shown):

```bash
ASSET=freemkv-cli-aarch64-macos
curl -sLO "https://github.com/freemkv/freemkv/releases/latest/download/${ASSET}"
curl -sLO "https://github.com/freemkv/freemkv/releases/latest/download/${ASSET}.sha256"
shasum -a 256 -c "${ASSET}.sha256"
chmod +x "${ASSET}"
sudo mv "${ASSET}" /usr/local/bin/freemkv
freemkv --version
```

## Windows

**App (installer).** Run `freemkv-x86_64-windows-setup.exe`. It installs for
the current user (no administrator rights) into
`%LOCALAPPDATA%\Programs\freemkv`, adds a Start menu shortcut, and puts the
folder on your user `PATH`. Uninstall from Settings → Apps.

**App (portable).** Unzip `freemkv-x86_64-windows.zip` and keep its two files
together:

- `freemkv.exe` — double-click to open the window.
- `freemkv.com` — the console entry point. Typing `freemkv` in cmd or
  PowerShell runs it (Windows tries `.com` before `.exe`), so commands print
  to the terminal. A bare `freemkv` opens the window and returns the prompt.

Add the folder to your `PATH` to run `freemkv` from anywhere.

**CLI.** Download `freemkv-cli-x86_64-windows.exe`, rename it to
`freemkv.exe`, and place it on your `PATH`.

## Linux

**App — Debian/Ubuntu** (Ubuntu 24.04, Linux Mint 22 or newer; needs GTK 4.10+
and libadwaita 1.4+):

```bash
curl -sLO https://github.com/freemkv/freemkv/releases/latest/download/freemkv-amd64.deb
sudo apt install ./freemkv-amd64.deb
```

This installs the package `freemkv` and replaces `freemkv-cli` if present.

**App — AppImage:**

```bash
curl -sLO https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-linux.AppImage
chmod +x freemkv-x86_64-linux.AppImage
./freemkv-x86_64-linux.AppImage
```

**App — Flatpak:**

```bash
curl -sLO https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-linux.flatpak
flatpak install --user ./freemkv-x86_64-linux.flatpak
flatpak run org.freemkv.FreeMKV
```

**CLI — Debian/Ubuntu:**

```bash
curl -sLO https://github.com/freemkv/freemkv/releases/latest/download/freemkv-cli-amd64.deb
sudo apt install ./freemkv-cli-amd64.deb
```

This installs the package `freemkv-cli` (no dependencies) and replaces the
`freemkv` app package if present.

**CLI — static binary** (x86_64 shown; use `freemkv-cli-aarch64-linux` on arm64):

```bash
ASSET=freemkv-cli-x86_64-linux
curl -sLO "https://github.com/freemkv/freemkv/releases/latest/download/${ASSET}"
curl -sLO "https://github.com/freemkv/freemkv/releases/latest/download/${ASSET}.sha256"
sha256sum -c "${ASSET}.sha256"
chmod +x "${ASSET}"
sudo mv "${ASSET}" /usr/local/bin/freemkv
freemkv --version
```

### Reading the optical drive without root

On Linux freemkv reads the drive via SCSI generic (`/dev/sr0` / the matching
`/dev/sg*`). Membership in the `cdrom` group is normally enough:

```bash
sudo usermod -aG cdrom "$USER"
# log out / back in (or `newgrp cdrom`) for the group to take effect
```

If your distro doesn't grant the `cdrom` group access to the SCSI generic
node, install a udev rule (see autorip's INSTALL.md for the rule text).

## Using it

Every operation is `freemkv <source> <dest>` over stream URLs:

```bash
freemkv disc:// mkv://Movie.mkv             # Disc → MKV
freemkv disc:// m2ts://Movie.m2ts            # Disc → raw transport stream
freemkv m2ts://Movie.m2ts mkv://Movie.mkv    # Remux m2ts → MKV
freemkv info disc://                         # Show disc info
```

### Decryption keys

- **DVD (CSS):** works out of the box, no setup.
- **Blu-ray + UHD (AACS):** require a `keydb.cfg`
  (default `~/.config/freemkv/keydb.cfg`). Fetch one and drop it there,
  or point `update-keys` at a URL:

  ```bash
  freemkv update-keys --url <keydb-url>
  ```

## Building from source

```bash
cargo build --release --bin freemkv                  # CLI
cargo build --release --bin freemkv --features gui   # app
```

The Linux app build needs the GTK4 and libadwaita development packages
(`libgtk-4-dev libadwaita-1-dev` on Debian/Ubuntu). On Windows, drop
`--bin freemkv` from the app build to also get `freemkv-gui.exe`: the release
ships it as `freemkv.exe`, beside the console image renamed to `freemkv.com`.
