#!/usr/bin/env bash
#
# cli-parity.sh — golden-master characterization of the freemkv CLI.
#
# The contract the `freemkv` CLI must satisfy. Originally recorded against the
# standalone CLI crate; now the gate that proved the unified `freemkv` crate
# (CLI + desktop shells over freemkv-engine) replicates it 1:1. Runs a matrix of
# invocations against $FREEMKV_BIN and captures exit-code + stdout + stderr,
# normalized so the golden is stable across builds. Then diffs against the
# recorded golden.
#
# Usage:
#   FREEMKV_BIN=/path/to/freemkv cli-parity.sh record   # write golden (run vs OLD cli)
#   FREEMKV_BIN=/path/to/new-cli cli-parity.sh check     # diff vs golden (run vs NEW cli)
#   cli-parity.sh check                                  # default bin = freemkv/target/release/freemkv
#
# Optional media fixtures (positive paths run only when set):
#   FREEMKV_TEST_ISO=/path/to/disc.iso    → `info iso://` + `iso://→mkv://` remux
#
# Deterministic surface (always runs, no media): version, help/usage, exit
# codes, URL validation errors, flag rejections, --language switch, channel
# discipline, --quiet. Live disc:// rips need the rig and are out of scope
# (parity there is structural — same engine calls).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# normalize() rewrites any absolute path to the checkout as <ROOT>, so the
# golden is stable wherever the repo lives — a CI runner's workspace path is
# not the developer's. The crate dir is that prefix now this harness lives in
# the public repo rather than a sibling of it.
ROOT="$CRATE_DIR"
BIN="${FREEMKV_BIN:-$CRATE_DIR/target/release/freemkv}"
GOLDEN_DIR="${GOLDEN_DIR:-$SCRIPT_DIR/cli-parity/golden}"
MODE="${1:-check}"
WORK="$(mktemp -d -t fmkv-parity.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

if [ ! -x "$BIN" ]; then
  echo "✗ FREEMKV_BIN not executable: $BIN" >&2
  echo "  build it first (cd freemkv && cargo build --release) or set FREEMKV_BIN" >&2
  exit 2
fi
mkdir -p "$GOLDEN_DIR"

# Normalize volatile substrings so the golden is build-stable:
#  - version label (freemkv X.Y.Z (gHASH))  → freemkv <VER>
#  - the work tempdir path                   → <TMP>
#  - any absolute path to the repo           → <ROOT>
normalize() {
  sed -E \
    -e 's/freemkv [0-9]+\.[0-9]+\.[0-9]+[0-9A-Za-z.+-]*( \(g[0-9a-f]+\))?/freemkv <VER>/g' \
    -e 's/^[0-9]+\.[0-9]+\.[0-9]+[0-9A-Za-z.+-]*( \(g[0-9a-f]+\))?$/<VER>/' \
    -e 's/ \(g[0-9a-f]{6,}\)/ (g<HASH>)/g' \
    -e "s#$WORK#<TMP>#g" \
    -e "s#$ROOT#<ROOT>#g" \
    -e 's/\x1b\[[0-9;]*m//g'
}

PASS=0; FAIL=0; RECORDED=0
FAILED_CASES=()

# run <case-name> -- <args...>
#   captures exit + normalized stdout + stderr into one golden blob.
run() {
  local name="$1"; shift
  [ "$1" = "--" ] && shift
  local out err code
  out="$("$BIN" "$@" 2>"$WORK/err")"; code=$?
  err="$(cat "$WORK/err")"
  local blob
  blob="$(printf 'EXIT=%s\n--- STDOUT ---\n%s\n--- STDERR ---\n%s\n' \
      "$code" "$out" "$err" | normalize)"
  local gf="$GOLDEN_DIR/$name.golden"
  if [ "$MODE" = "record" ]; then
    printf '%s\n' "$blob" > "$gf"
    RECORDED=$((RECORDED+1))
  else
    if [ ! -f "$gf" ]; then
      echo "✗ $name: no golden (run 'record' first)"; FAIL=$((FAIL+1)); FAILED_CASES+=("$name"); return
    fi
    if diff -q <(printf '%s\n' "$blob") "$gf" >/dev/null; then
      PASS=$((PASS+1))
    else
      echo "✗ $name: differs from golden"
      diff <(printf '%s\n' "$blob") "$gf" | head -20 | sed 's/^/    /'
      FAIL=$((FAIL+1)); FAILED_CASES+=("$name")
    fi
  fi
}

echo "── cli-parity ($MODE) bin=$BIN ──"

# ── Version / help / usage / exit codes ─────────────────────────────────────
run version_word       -- version
run version_longflag   -- --version
run version_shortflag  -- -V
run help_word          -- help
run help_longflag      -- --help
run help_shortflag     -- -h
run help_info          -- help info
run help_update_keys   -- help update-keys
run info_dashhelp      -- info --help
run updatekeys_dashhelp -- update-keys --help
run help_unknown_cmd   -- help nonsense
run bare_no_args       --

# ── URL validation / error surface ──────────────────────────────────────────
run schemeless_source        -- notaurl mkv://out.mkv
run schemeless_dest          -- iso:///nonexistent.iso /path/out.mkv
run bad_scheme               -- floppy://x mkv://out.mkv
run missing_iso              -- iso:///no_such_file.iso mkv://out.mkv
run nonexistent_drive        -- disc:///dev/sg999 mkv://out.mkv
run null_input_rejected      -- null:// mkv://out.mkv
run raw_into_mkv_rejected    -- iso:///x.iso mkv://out.mkv --raw
run multipass_into_mkv_rej   -- iso:///x.iso mkv://out.mkv --multipass
run unknown_flag             -- iso:///x.iso mkv://out.mkv --bogus
run info_no_url              -- info
run info_unknown_scheme      -- info floppy://x
run single_bad_url           -- notaurl

# ── i18n: --language switches localized output (usage differs from en) ───────
run usage_en                 -- help
run usage_de                 -- --language de help
run usage_fr                 -- --lang fr help
run bad_language_value       -- --language mkv://out.mkv

# ── Media-dependent positive paths (only when a fixture is provided) ─────────
if [ -n "${FREEMKV_TEST_ISO:-}" ] && [ -f "${FREEMKV_TEST_ISO}" ]; then
  cp "$FREEMKV_TEST_ISO" "$WORK/fixture.iso"
  # info on an ISO — title listing (normalize the fixture path).
  ISO_OUT="$("$BIN" info "iso://$WORK/fixture.iso" 2>&1 | normalize | sed "s#$WORK/fixture.iso#<ISO>#g")"
  if [ "$MODE" = "record" ]; then printf '%s\n' "$ISO_OUT" > "$GOLDEN_DIR/info_iso.golden"; RECORDED=$((RECORDED+1))
  else
    if diff -q <(printf '%s\n' "$ISO_OUT") "$GOLDEN_DIR/info_iso.golden" >/dev/null; then PASS=$((PASS+1))
    else echo "✗ info_iso differs"; FAIL=$((FAIL+1)); FAILED_CASES+=("info_iso"); fi
  fi
else
  echo "  (skipping media positive-path cases — set FREEMKV_TEST_ISO to enable)"
fi

echo
if [ "$MODE" = "record" ]; then
  echo "── recorded $RECORDED golden cases → $GOLDEN_DIR ──"
else
  echo "── parity: $PASS ok, $FAIL differ ──"
  [ "$FAIL" -eq 0 ] || { printf '   failed: %s\n' "${FAILED_CASES[*]}"; exit 1; }
fi
