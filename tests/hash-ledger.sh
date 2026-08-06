#!/usr/bin/env bash
#
# hash-ledger.sh — a memory for the acceptance suites.
#
# The suites prove each output is correct on the host that produced it. They do
# not remember what they produced last time, so a change in output between
# releases is invisible unless someone happens to be looking at the right file.
# This records it.
#
#   hash-ledger.sh check  <key> <file>        compare against the ledger
#   hash-ledger.sh accept <key> --reason "…"  record the pending hash as correct
#   hash-ledger.sh report                     summarise the run
#
# ── Why a changed hash is ambiguous, and what to do about it ────────────────
#
# A content hash that changes means one of two opposite things:
#
#   - a REGRESSION: output changed because something broke;
#   - a FIX LANDING: output changed because it was wrong before.
#
# Both produce an identical signal, and that ambiguity is the whole problem. A
# real example from 1.6.1: CSS descrambling used to trigger on byte 0x14 alone,
# which is not a scramble flag outside a VOB pack, so it corrupted 1912 bytes of
# every DVD's VIDEO_TS.IFO. Fixing it changes the output hash of every CSS DVD
# fixture, and every one of those changes is an improvement.
#
# So a changed hash FAILS by default and is never overwritten as a side effect
# of re-running. Accepting one is a separate, deliberate act that records a
# reason in prose. The superseded hash stays in the ledger: "this changed at
# 1.6.1 because CSS stopped corrupting IFO sectors" is the sentence that makes
# the NEXT unexplained change diagnosable.
#
# A ledger that silently absorbs diffs gets ignored within a week, and an
# ignored ledger is worse than none because it looks like coverage.
#
# ── What is hashed ─────────────────────────────────────────────────────────
#
# NOT the file bytes. libfreemkv writes MuxingApp/WritingApp = "freemkv
# <version> (g<sha>)" into every MKV, so a whole-file hash changes on every
# commit and proves nothing — a ledger that is red every run is the same as no
# ledger.
#
# Instead: the framecrc muxer over the copied elementary streams. That is a
# per-packet CRC with presentation and decode timestamps, so it covers stream
# CONTENT and TIMING while ignoring container metadata. Timing is not incidental
# — the other defect fixed in 1.6.1 was multi-clip A/V drift, where the streams
# were individually intact and their timestamps were not. A payload-only hash
# would have slept through it.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LEDGER="${FMKV_LEDGER:-$SCRIPT_DIR/hash-ledger.json}"
STATE="${FMKV_LEDGER_STATE:-${TMPDIR:-/tmp}/fmkv-ledger-state.json}"

command -v ffmpeg >/dev/null 2>&1 || { echo "hash-ledger: ffmpeg not found" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "hash-ledger: python3 not found" >&2; exit 2; }

# Content hash: per-packet CRCs of the streams, container metadata excluded.
content_hash() {
  local f="$1"
  ffmpeg -v error -i "$f" -map 0 -c copy -f framecrc - 2>/dev/null \
    | { command -v sha256sum >/dev/null 2>&1 && sha256sum || shasum -a 256; } \
    | cut -d' ' -f1
}

VERSION="${FMKV_VERSION:-unknown}"
COMMIT="${FMKV_COMMIT:-unknown}"
STAMP="${FMKV_STAMP:-unknown}"

py() { python3 - "$LEDGER" "$STATE" "$@"; }

case "${1:-}" in
check)
  key="${2:?key}"; file="${3:?file}"
  if [ ! -f "$file" ]; then
    py miss "$key" <<'EOF'
import json,sys,os
ledger,state,_,key=sys.argv[1:5]
s=json.load(open(state)) if os.path.exists(state) else {"new":[],"same":[],"changed":[],"absent":[]}
s["absent"].append(key)
json.dump(s,open(state,"w"),indent=2)
print(f"  ABSENT  {key} — fixture not produced, so nothing was compared")
EOF
    exit 0
  fi
  h="$(content_hash "$file")"
  py cmp "$key" "$h" "$VERSION" "$COMMIT" "$STAMP" <<'EOF'
import json,sys,os
ledger,state,_,key,h,version,commit,stamp=sys.argv[1:9]
L=json.load(open(ledger)) if os.path.exists(ledger) else {"entries":{}}
S=json.load(open(state)) if os.path.exists(state) else {"new":[],"same":[],"changed":[],"absent":[]}
e=L["entries"].get(key)
if e is None:
    # Not yet recorded is NOT a regression. Conflating the two trains people to
    # accept diffs, which is how a ledger stops meaning anything.
    L["entries"][key]={"hash":h,"version":version,"commit":commit,"recorded":stamp,
                       "reason":"first recording — no prior value to compare against",
                       "history":[]}
    json.dump(L,open(ledger,"w"),indent=2,sort_keys=True)
    S["new"].append(key); print(f"  RECORD  {key} — new baseline {h[:12]}")
elif e["hash"]==h:
    S["same"].append(key); print(f"  MATCH   {key} {h[:12]}")
else:
    S["changed"].append({"key":key,"was":e["hash"],"now":h,
                         "since_version":e["version"],"since":e["recorded"]})
    print(f"  CHANGED {key}")
    print(f"          was {e['hash'][:16]}  ({e['version']}, {e['recorded']})")
    print(f"          now {h[:16]}")
    print(f"          -> regression, or a fix landing? Decide, then:")
    print(f"             tests/hash-ledger.sh accept {key} --reason \"...\"")
json.dump(S,open(state,"w"),indent=2)
EOF
  # A changed hash fails the run. Accepting it is a separate act.
  py failed <<'EOF'
import json,sys,os
ledger,state=sys.argv[1:3]
S=json.load(open(state)) if os.path.exists(state) else {"changed":[]}
sys.exit(1 if S.get("changed") else 0)
EOF
  ;;

accept)
  key="${2:?key}"; shift 2
  reason=""
  while [ $# -gt 0 ]; do case "$1" in --reason) reason="${2:-}"; shift 2;; *) shift;; esac; done
  [ -n "$reason" ] || { echo "hash-ledger: accept needs --reason \"why this changed\"" >&2; exit 2; }
  py acc "$key" "$reason" "$VERSION" "$COMMIT" "$STAMP" <<'EOF'
import json,sys,os
ledger,state,_,key,reason,version,commit,stamp=sys.argv[1:9]
S=json.load(open(state)) if os.path.exists(state) else {"changed":[]}
c=next((x for x in S.get("changed",[]) if x["key"]==key),None)
if not c:
    print(f"hash-ledger: no pending change for {key} — run `check` first",file=sys.stderr); sys.exit(2)
L=json.load(open(ledger)); e=L["entries"][key]
# The superseded value is kept, not replaced. History is the point.
e.setdefault("history",[]).append({k:e[k] for k in ("hash","version","commit","recorded","reason")})
e.update(hash=c["now"],version=version,commit=commit,recorded=stamp,reason=reason)
json.dump(L,open(ledger,"w"),indent=2,sort_keys=True)
print(f"  ACCEPT  {key} -> {c['now'][:12]}\n          {reason}")
EOF
  ;;

report)
  py rep <<'EOF'
import json,sys,os
ledger,state=sys.argv[1:3]
S=json.load(open(state)) if os.path.exists(state) else {"new":[],"same":[],"changed":[],"absent":[]}
n,s,c,a=len(S["new"]),len(S["same"]),len(S["changed"]),len(S["absent"])
print(f"\n-- hash ledger: {s} match, {n} newly recorded, {c} CHANGED, {a} absent --")
if n and not s:
    # Say this out loud. A first run reporting "green" while it actually
    # recorded every baseline is the false confidence this exists to remove.
    print("   NOTE: nothing was compared — this run SEEDED the ledger.")
for k in S["absent"]:
    print(f"   absent: {k} (coverage gap, not a pass)")
for x in S["changed"]:
    print(f"   changed: {x['key']}  since {x['since_version']}")
sys.exit(1 if c else 0)
EOF
  ;;

reset) rm -f "$STATE" ;;
*) sed -n '3,10p' "$0"; exit 2 ;;
esac
