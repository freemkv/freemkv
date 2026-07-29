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

# Ad-hoc signature. Not notarized, so first launch still needs right-click →
# Open (the download page says so); with no signature at all Gatekeeper reports
# the vaguer "app is damaged" instead.
codesign --force --deep --sign - "$APP" 2>/dev/null || true
echo "built $APP ($(lipo -archs "$APP/Contents/MacOS/freemkv"))"
