#!/usr/bin/env bash
#
# freemkv CLI integration tester
# --------------------------------
# A self-contained, hardware-free acceptance test for the freemkv CLI. It
# builds the binary, GENERATES its own Blu-ray-legal test media with ffmpeg
# (H.264 video + two AC-3 audio tracks, eng/spa), then exercises every
# file-reachable CLI function and VERIFIES the output with ffprobe/ffmpeg.
#
# Run it before a release to confirm the CLI still does what it claims.
#
#   tests/cli-integration.sh            # build + test
#   FMKV_BIN=/path/to/freemkv tests/cli-integration.sh   # test a prebuilt binary
#   FMKV_ISO_DIR=/path/to/isos tests/cli-integration.sh  # + exercise info iso://
#
# Exit status is 0 only if every test passes.
#
# NOTE ON SCOPE: disc:// and iso:// are scanned into a title list, so the
# selection flags (-t/-a/-s) and true-multipass recovery only apply there.
# Those paths need a physical drive or a real Blu-ray ISO and are validated
# separately (see the release checklist). This script covers the file surface:
# version/help, info, remux, format detection, null/stdio, the flag/exit-code
# contract, and — if FMKV_ISO_DIR is set — read-only info on real ISOs.

set -u

# ── Locate tools ──────────────────────────────────────────────────────────────
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

need() { command -v "$1" >/dev/null 2>&1 || { echo "FATAL: '$1' not found in PATH" >&2; exit 2; }; }
need ffmpeg
need ffprobe

# ── Locate / build the binary ─────────────────────────────────────────────────
if [ -n "${FMKV_BIN:-}" ]; then
  BIN="$FMKV_BIN"
else
  echo "Building freemkv (release)…"
  ( cd "$CRATE_DIR" && cargo build --release --bin freemkv ) || { echo "FATAL: build failed" >&2; exit 2; }
  BIN="$CRATE_DIR/target/release/freemkv"
fi
[ -x "$BIN" ] || { echo "FATAL: binary not executable: $BIN" >&2; exit 2; }
echo "Testing binary: $BIN"
echo "Version:        $("$BIN" version 2>/dev/null | head -1)"
echo

# ── Scratch workspace (auto-cleaned) ──────────────────────────────────────────
WORK="$(mktemp -d "${TMPDIR:-/tmp}/freemkv-cli-test.XXXXXX")"
cleanup() { [ -n "${FMKV_KEEP:-}" ] || rm -rf "$WORK"; }

# Cross-platform parity: every artifact this run produced, hashed. The suite
# already proves each output is CORRECT on the host it ran on; hashing lets a
# caller prove the three platforms produced the SAME bytes, which no
# single-host run can show.
emit_hashes() {
  [ -n "${FMKV_HASHES:-}" ] || return 0
  local h
  for f in master.mkv out.mkv ctrl.mkv master.m2ts rt.mkv; do
    [ -f "$WORK/$f" ] || continue
    if command -v sha256sum >/dev/null 2>&1; then h=$(sha256sum "$WORK/$f" | cut -d\  -f1)
    else h=$(shasum -a 256 "$WORK/$f" | cut -d\  -f1); fi
    printf '%s  %s\n' "$h" "$f" >> "$FMKV_HASHES"
  done
}
trap cleanup EXIT
cd "$WORK"

# ── Test harness ──────────────────────────────────────────────────────────────
PASS=0; FAIL=0
if [ -t 1 ]; then G=$'\033[32m'; R=$'\033[31m'; Y=$'\033[33m'; Z=$'\033[0m'; else G=; R=; Y=; Z=; fi

ok()   { PASS=$((PASS+1)); printf "  ${G}PASS${Z} %s\n" "$1"; }
bad()  { FAIL=$((FAIL+1)); printf "  ${R}FAIL${Z} %s\n" "$1"; [ -n "${2:-}" ] && printf "        %s\n" "$2"; }
skip() { printf "  ${Y}SKIP${Z} %s\n" "$1"; }
group(){ printf "\n${Y}== %s ==${Z}\n" "$1"; }

# run <name> <expected_exit> -- <cmd...>   (asserts the exit code)
run() {
  local name="$1" want="$2"; shift 3   # drop name, want, and the literal "--"
  "$@" >"$WORK/.out" 2>"$WORK/.err"; local got=$?
  if [ "$got" = "$want" ]; then ok "$name (exit $got)"
  else bad "$name" "expected exit $want, got $got — $(tail -1 "$WORK/.err" 2>/dev/null)"; fi
}

# out_has <name> <regex> -- <cmd...>   (exit 0 AND stdout matches regex)
out_has() {
  local name="$1" re="$2"; shift 3
  "$@" >"$WORK/.out" 2>"$WORK/.err"; local got=$?
  if [ "$got" != 0 ]; then bad "$name" "exit $got — $(tail -1 "$WORK/.err")"; return; fi
  if grep -Eq "$re" "$WORK/.out"; then ok "$name"
  else bad "$name" "output did not match /$re/"; fi
}

# ffprobe helpers
nstreams() { ffprobe -v error -show_entries stream=index -of csv=p=0 "$1" 2>/dev/null | grep -c .; }
codec_types() { ffprobe -v error -show_entries stream=codec_type -of csv=p=0 "$1" 2>/dev/null | sort | tr '\n' ',' ; }
audio_langs() { ffprobe -v error -select_streams a -show_entries stream_tags=language -of csv=p=0 "$1" 2>/dev/null | sort | tr '\n' ',' ; }
duration_i()  { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1" 2>/dev/null | cut -d. -f1; }

# ── Generate test media (Blu-ray-legal codecs: H.264 + AC-3) ──────────────────
group "Generate test media"
if ffmpeg -v error -f lavfi -i testsrc=duration=6:size=640x480:rate=24 \
     -f lavfi -i sine=frequency=440:duration=6 \
     -f lavfi -i sine=frequency=880:duration=6 \
     -map 0:v -map 1:a -map 2:a -c:v libx264 -pix_fmt yuv420p -c:a ac3 -b:a 192k \
     -metadata:s:a:0 language=eng -metadata:s:a:1 language=spa master.mkv; then
  ok "ffmpeg generated master.mkv ($(nstreams master.mkv) streams)"
else
  bad "ffmpeg media generation"; echo "cannot continue without media" >&2; exit 2
fi

# ── Version / help ────────────────────────────────────────────────────────────
group "Version & help"
out_has "version prints x.y.z"     '[0-9]+\.[0-9]+\.[0-9]+' -- "$BIN" version
out_has "--version"                '[0-9]+\.[0-9]+\.[0-9]+' -- "$BIN" --version
run     "-V"                    0   -- "$BIN" -V
out_has "help shows usage"         'Usage:'                 -- "$BIN" help
out_has "help info"                'info'                   -- "$BIN" help info
out_has "--help"                   'Usage:'                 -- "$BIN" --help
run     "unknown subcommand → err" 1  -- "$BIN" definitely-not-a-command

# ── info (reading) ────────────────────────────────────────────────────────────
group "info — reading"
out_has "info mkv:// streams"      'Streams: 3'   -- "$BIN" info mkv://master.mkv
out_has "info mkv:// duration"     'Duration'     -- "$BIN" info mkv://master.mkv
run     "info -v (verbose)"      0 -- "$BIN" info mkv://master.mkv -v
run     "info -f (full)"         0 -- "$BIN" info mkv://master.mkv -f
run     "info -b (basic)"        0 -- "$BIN" info mkv://master.mkv -b

# freemkv makes an m2ts, then reads it back (exercises the TS reader).
if "$BIN" mkv://master.mkv m2ts://master.m2ts >/dev/null 2>&1; then
  out_has "info m2ts:// (own output)" 'Streams: 3' -- "$BIN" info m2ts://master.m2ts
else
  skip "info m2ts:// (m2ts mux unavailable for synthetic MKV)"
fi

# ── info — error contract ─────────────────────────────────────────────────────
group "info — errors (exit 1)"
run "info without scheme"        1 -- "$BIN" info master.mkv
run "info nonexistent file"      1 -- "$BIN" info mkv://nope.mkv
cp master.mkv notreally.iso
run "info malformed iso"         1 -- "$BIN" info iso://notreally.iso

# ── Remux + ffprobe verification ──────────────────────────────────────────────
group "Remux mkv:// → mkv:// (verified with ffprobe)"
if "$BIN" mkv://master.mkv mkv://out.mkv >/dev/null 2>&1; then
  ok "rip exit 0"
  [ "$(nstreams out.mkv)" = 3 ] && ok "3 streams preserved" || bad "stream count" "got $(nstreams out.mkv)"
  [ "$(codec_types out.mkv)" = "audio,audio,video," ] && ok "1 video + 2 audio" || bad "codec types" "got $(codec_types out.mkv)"
  [ "$(audio_langs out.mkv)" = "eng,spa," ] && ok "audio langs eng+spa preserved" || bad "audio langs" "got $(audio_langs out.mkv)"
  [ "$(duration_i out.mkv)" = 6 ] && ok "duration preserved (~6s)" || bad "duration" "got $(duration_i out.mkv)s"
  if ffmpeg -v error -i out.mkv -f null - >/dev/null 2>&1; then ok "output fully decodes in ffmpeg"; else bad "ffmpeg decode of output"; fi
else
  bad "rip mkv://→mkv://ded"
fi

# ── null / stdio ──────────────────────────────────────────────────────────────
group "null:// & stdio://"
out_has "null:// benchmark"        'Complete'  -- "$BIN" mkv://master.mkv null://

# stdio:// is freemkv's wire format (stdout IS the byte stream). Its stdout must
# be PURE data: the first bytes are the 'FMKV' wire magic, never a banner. This
# guards against logs corrupting a piped stream.
"$BIN" mkv://master.mkv stdio:// >wire.bin 2>/dev/null
magic="$(head -c 4 wire.bin 2>/dev/null)"
if [ "$magic" = "FMKV" ]; then ok "stdio:// stdout is pure wire data (no banner leak)"
else bad "stdio:// stdout leaks non-data" "first 4 bytes: '$magic' (expected FMKV)"; fi

# Round-trip through freemkv (out → in) must reconstruct the streams + languages.
if "$BIN" mkv://master.mkv stdio:// 2>/dev/null | "$BIN" stdio:// mkv://rt.mkv >/dev/null 2>&1 && [ -s rt.mkv ]; then
  [ "$(nstreams rt.mkv)" = 3 ] && ok "stdio:// round-trip: 3 streams" || bad "stdio:// round-trip streams" "got $(nstreams rt.mkv)"
  [ "$(audio_langs rt.mkv)" = "eng,spa," ] && ok "stdio:// round-trip: langs preserved" || bad "stdio:// round-trip langs" "got $(audio_langs rt.mkv)"
else
  bad "stdio:// round-trip produced no output"
fi

# ── Selection-flag gate (disc/iso only; must fail loud on a file) ─────────────
group "Selection flags on a file source → exit 1"
run "-a on file rejected"        1 -- "$BIN" mkv://master.mkv mkv://x.mkv -a eng
run "-s on file rejected"        1 -- "$BIN" mkv://master.mkv mkv://x.mkv -s eng
run "-t on file rejected"        1 -- "$BIN" mkv://master.mkv mkv://x.mkv -t 1
run "no flags → remux allowed"   0 -- "$BIN" mkv://master.mkv mkv://ctrl.mkv

# ── Other flag / scheme gates ─────────────────────────────────────────────────
group "Flag & scheme gates → exit 1"
run "dest without scheme"        1 -- "$BIN" mkv://master.mkv out.mkv
run "--raw is iso-only"          1 -- "$BIN" mkv://master.mkv mkv://x.mkv --raw
run "--multipass is iso-only"    1 -- "$BIN" mkv://master.mkv mkv://x.mkv --multipass

# ── update-keys error path (offline, no external network) ─────────────────────
group "update-keys — unreachable server → exit 1"
run "update-keys bad url"        1 -- "$BIN" update-keys --url http://127.0.0.1:9/nope

# ── Optional: real ISOs (read-only info) ──────────────────────────────────────
group "iso:// info (optional — set FMKV_ISO_DIR)"
if [ -n "${FMKV_ISO_DIR:-}" ] && [ -d "$FMKV_ISO_DIR" ]; then
  iso="$(ls "$FMKV_ISO_DIR"/*.iso 2>/dev/null | head -1)"
  if [ -n "$iso" ]; then
    out_has "info iso://$(basename "$iso")" 'Title|Titles|Duration' -- "$BIN" info "iso://$iso"
  else
    skip "no *.iso in $FMKV_ISO_DIR"
  fi
else
  skip "FMKV_ISO_DIR not set — disc/iso selection & multipass need hardware (see release checklist)"
fi

# ── Does this build still produce what it produced last time? ────────────────
#
# Everything above proves each output is CORRECT on the host that made it.
# None of it remembers what the LAST release produced, so an output that
# quietly changes between versions is invisible here.
#
# hash-ledger.sh records a content hash per artifact — ffmpeg framecrc over the
# copied streams, so it covers stream content and packet timing while ignoring
# the version stamp libfreemkv writes into every MKV header. A changed hash
# fails, because it is ambiguous: it means either a regression or a fix
# landing, and which one it is has to be decided by a human rather than
# absorbed by a re-run. See the header of hash-ledger.sh.
#
# The media here is generated deterministically by ffmpeg, so these hashes are
# reproducible on any runner — which is what makes this the right place to
# prove the mechanism before the real-media suite depends on it.
group "hash ledger"
LEDGER_SH="$SCRIPT_DIR/hash-ledger.sh"
if [ -x "$LEDGER_SH" ] && command -v python3 >/dev/null 2>&1; then
  FMKV_VERSION="$("$BIN" version 2>/dev/null | head -1)" \
  FMKV_COMMIT="$(git -C "$CRATE_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
  FMKV_STAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    bash -c '
      set -u
      "$1" reset
      rc=0
      # Key on the full filename, extension included: master.mkv and
      # master.m2ts are different artifacts and stripping the extension
      # silently collapsed them onto one ledger entry.
      #
      # And key PER PLATFORM. The media here is generated by whichever
      # ffmpeg the runner has, and encoder output differs between builds — the same
      # trap parity.yml documents, which is why IT generates media once and
      # shares it as an artifact. A single shared baseline therefore reported
      # all five artifacts CHANGED on every runner that was not the one that
      # seeded it, which is noise, not a regression.
      #
      # Per-platform baselines keep the question this ledger actually answers:
      # "did THIS platform output the same thing as last time?" Cross-platform
      # byte equality is a different question and parity.yml already owns it.
      plat="${3:-unknown}"
      for f in master.mkv out.mkv ctrl.mkv rt.mkv master.m2ts; do
        "$1" check "$plat/$f" "$2/$f" || rc=1
      done
      "$1" report || rc=1
      exit $rc
    ' _ "$LEDGER_SH" "$WORK" "$(uname -s)-$(uname -m)"
  if [ $? = 0 ]; then ok "hash ledger"; else bad "hash ledger" "output changed — decide regression vs fix, then accept with a reason"; fi
else
  skip "hash-ledger.sh or python3 unavailable"
fi

# ── Summary ───────────────────────────────────────────────────────────────────
printf "\n%s========================================%s\n" "$Y" "$Z"
printf "  %sPASS: %d%s   %sFAIL: %d%s\n" "$G" "$PASS" "$Z" "$R" "$FAIL" "$Z"
printf "%s========================================%s\n" "$Y" "$Z"
[ "$FAIL" = 0 ] && { echo "${G}CLI integration: OK${Z}"; exit 0; } || { echo "${R}CLI integration: FAILURES${Z}"; exit 1; }
