#!/bin/bash
# Assemble freemkv.app from the built binary.
#
#   bundle.sh                       local build, debug, current arch
#   bundle.sh release               local build, release, current arch
#   bundle.sh release <rust-target> a specific cross-built target
#
# The release workflow ships one .app per architecture, matching the CLI's two
# macOS flavors (Apple Silicon and Intel).
set -e
cd "$(dirname "$0")/.."
PROFILE=${1:-debug}
TARGET=${2:-}
APP="target/freemkv.app"
rm -rf "$APP" macos/freemkv.iconset
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" macos/freemkv.iconset

for s in 16 32 64 128 256 512; do
  sips -z $s $s macos/freemkv.png --out "macos/freemkv.iconset/icon_${s}x${s}.png" >/dev/null
  d=$((s*2))
  sips -z $d $d macos/freemkv.png --out "macos/freemkv.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns macos/freemkv.iconset -o "$APP/Contents/Resources/freemkv.icns"
rm -rf macos/freemkv.iconset

cp macos/Info.plist "$APP/Contents/Info.plist"

# Build first so the bundle can never ship a stale binary. (`precommit` only
# builds debug, so relying on a pre-existing target/release/freemkv silently
# bundled whatever was last compiled — a real footgun during QA.)
BUILD=(cargo build --bin freemkv)
[ "$PROFILE" = release ] && BUILD+=(--release)
[ -n "$TARGET" ] && BUILD+=(--target "$TARGET")
"${BUILD[@]}"

if [ -n "$TARGET" ]; then
  BIN="target/$TARGET/$PROFILE/freemkv"
else
  BIN="target/$PROFILE/freemkv"
fi
[ -f "$BIN" ] || { echo "missing $BIN — build failed?" >&2; exit 1; }
cp "$BIN" "$APP/Contents/MacOS/freemkv"

# Sign with a Developer ID when one is available, ad-hoc when it is not.
#
# MACOS_SIGN_IDENTITY is set by the release workflow after it imports the
# Developer ID certificate; a local build (and any fork's CI, which cannot read
# secrets) leaves it empty and gets the ad-hoc signature this always did. An
# ad-hoc signature is what stops Gatekeeper reporting the vaguer "app is
# damaged" on arm64, where an unsigned binary will not load at all — but it is
# NOT a distributable signature, and only a Developer ID one can be notarized.
#
# Signed inner-out and WITHOUT --deep: --deep is deprecated, and it re-signs
# nested code with the outer options rather than the ones each part needs, so
# it is the standard way to end up with a bundle that notarization rejects.
# --options runtime (the hardened runtime) and --timestamp are both mandatory
# for notarization; a signature missing either is refused with no useful error.
if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp \
    --sign "$MACOS_SIGN_IDENTITY" "$APP/Contents/MacOS/freemkv"
  codesign --force --options runtime --timestamp \
    --sign "$MACOS_SIGN_IDENTITY" "$APP"
  codesign --verify --strict --verbose=2 "$APP"
  echo "signed with Developer ID: $MACOS_SIGN_IDENTITY"
else
  codesign --force --sign - "$APP/Contents/MacOS/freemkv" 2>/dev/null || true
  codesign --force --sign - "$APP" 2>/dev/null || true
  echo "ad-hoc signed (no MACOS_SIGN_IDENTITY) — not distributable"
fi
echo "built $APP ($(lipo -archs "$APP/Contents/MacOS/freemkv"))"
