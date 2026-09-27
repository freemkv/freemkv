#!/usr/bin/env bash
# The one Rust toolchain (TC) for every media-evidence and shipped build.
#
#   exact-toolchain.sh install <Cargo.toml> [target...]
#       Reads freemkv's rust-version, which must be an exact X.Y.Z, installs it
#       with rustup, exports RUSTUP_TOOLCHAIN, and asserts rustc reports it.
#       TC_COMPONENTS="clippy,rustfmt" adds components.
#   exact-toolchain.sh assert-env <target> [crate-dir]
#       Fails if the environment or a cargo config could change the compiled
#       code, then prints the C toolchain the target's C code builds with.
#
# A 2-part value is rejected: rustup resolves "1.98" to the newest 1.98.x at
# install time, so the build would silently float.
set -euo pipefail

die() { echo "::error title=$1::$2" >&2; exit 1; }

read_tc() {
  local v
  [ -f "$1" ] || die "No manifest" "$1 not found"
  v=$(sed -n -E 's/^rust-version[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' "$1" | head -1)
  [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || die "Inexact toolchain" "$1: rust-version must be an exact X.Y.Z (got '$v')"
  echo "$v"
}

install() {
  local manifest="$1" v got comps=()
  shift
  v=$(read_tc "$manifest")
  local args=()
  for t in "$@"; do args+=(--target "$t"); done
  [ -z "${TC_COMPONENTS:-}" ] || comps=(--component "$TC_COMPONENTS")
  rustup toolchain install "$v" --profile minimal --no-self-update \
    ${args[@]+"${args[@]}"} ${comps[@]+"${comps[@]}"}
  got=$(RUSTUP_TOOLCHAIN="$v" rustc -Vv | sed -n 's/^release: //p' | tr -d '\r')
  [ "$got" = "$v" ] || die "Wrong rustc" "RUSTUP_TOOLCHAIN=$v runs rustc release '$got'"
  if [ -n "${GITHUB_ENV:-}" ]; then echo "RUSTUP_TOOLCHAIN=$v" >> "$GITHUB_ENV"; fi
  if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "version=$v" >> "$GITHUB_OUTPUT"; fi
  RUSTUP_TOOLCHAIN="$v" rustc -Vv
  RUSTUP_TOOLCHAIN="$v" cargo -V
}

# Variables that change what rustc or the C compilers (cc-rs) emit.
DIRTY_RE='^(FREEMKV_BUILD_LABEL|FREEMKV_GH_TOKEN|RUSTFLAGS|CARGO_ENCODED_RUSTFLAGS|CARGO_BUILD_RUSTFLAGS|RUSTC|RUSTC_WRAPPER|RUSTC_WORKSPACE_WRAPPER|CARGO_BUILD_RUSTC|CARGO_BUILD_RUSTC_WRAPPER|CARGO_PROFILE_RELEASE_[A-Z0-9_]+|CARGO_TARGET_[A-Z0-9_]+_(RUSTFLAGS|LINKER|RUNNER)|(TARGET_|HOST_)?(CC|CFLAGS|CXX|CXXFLAGS|AR|ARFLAGS)|(CC|CFLAGS|CXX|CXXFLAGS|AR|ARFLAGS)_[A-Za-z0-9_.-]+)$'

# Cargo config keys and tables that change the compiled code.
CONFIG_RE='^[[:space:]]*(\[(build|env|profile|target)([].]|$)|(rustflags|rustdocflags|linker|rustc|rustc-wrapper|rustc-workspace-wrapper|ar)[[:space:]]*=)'

# The C compiler cc-rs resolves for the target, and its version.
c_toolchain() {
  local target="$1" c vswhere inst ver
  case "$target" in
    aarch64-unknown-linux-musl)
      # Built through `cross`: the compiler is the pinned image's.
      echo "cross image ${CROSS_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_IMAGE:-<unpinned>}" ;;
    *-linux-musl)
      # cc-rs compiles x86_64 musl C with musl-gcc (the musl-tools wrapper).
      command -v musl-gcc >/dev/null 2>&1 || return 1
      echo "musl-gcc: $(musl-gcc --version | head -1)"
      if command -v dpkg-query >/dev/null 2>&1; then
        echo "musl-tools: $(dpkg-query -W -f='${Version}' musl-tools 2>/dev/null || echo unknown)"
      fi ;;
    *-windows-msvc)
      vswhere="/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"
      [ -x "$vswhere" ] || return 1
      inst=$("$vswhere" -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 \
               -property installationPath | tr -d '\r')
      [ -n "$inst" ] || return 1
      ver=$(tr -d '\r' < "$(cygpath -u "$inst")/VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt")
      echo "msvc: VC tools $ver ($inst)" ;;
    *)
      command -v cc >/dev/null 2>&1 || return 1
      echo "cc: $(cc --version | head -1)" ;;
  esac
}

assert_env() {
  local target="$1" dir="${2:-.}" bad d f hits c
  bad=$(env | sed -n 's/^\([A-Za-z_][A-Za-z0-9_.-]*\)=.*/\1/p' | grep -E "$DIRTY_RE" | sort -u || true)
  [ -z "$bad" ] || die "Build environment not clean" "these variables change the compiled code: $(tr "\n" " " <<<"$bad")"
  # Config files cargo reads for a build in $dir: its ancestors and CARGO_HOME.
  # Only [patch] (the sibling redirect) and [net]/[http]-style tables may appear.
  local files=() home="${CARGO_HOME:-$HOME/.cargo}"
  d=$(cd "$dir" && pwd)
  while :; do
    files+=("$d/.cargo/config" "$d/.cargo/config.toml")
    [ "$d" != "$(dirname "$d")" ] || break
    d=$(dirname "$d")
  done
  files+=("$home/config" "$home/config.toml")
  for f in "${files[@]}"; do
    [ -f "$f" ] || continue
    hits=$(grep -nE "$CONFIG_RE" "$f" || true)
    [ -z "$hits" ] || die "Cargo config changes the build" "$f: $(tr "\n" " " <<<"$hits")"
  done
  c=$(c_toolchain "$target") || die "No C toolchain" "no C compiler found for $target"
  echo "$c"
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    { echo "c_toolchain<<EOF_C"; echo "$c"; echo "EOF_C"; } >> "$GITHUB_OUTPUT"
  fi
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    { echo "**$target** C toolchain:"; echo '```'; echo "$c"; echo '```'; } >> "$GITHUB_STEP_SUMMARY"
  fi
}

case "${1:-}" in
  install)    shift; [ $# -ge 1 ] || die "Usage" "install <Cargo.toml> [target...]"; install "$@" ;;
  assert-env) shift; [ $# -ge 1 ] || die "Usage" "assert-env <target> [crate-dir]"; assert_env "$@" ;;
  version)    shift; read_tc "$1" ;;
  *) die "Usage" "exact-toolchain.sh install|assert-env|version ..." ;;
esac
