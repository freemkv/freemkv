#!/bin/bash
# Install the native Library's login service without interrupting active jobs.
set -euo pipefail
if [[ $(uname -s) != Darwin || $# -lt 1 || $# -gt 2 ]]; then
  echo "Usage (macOS): $0 /absolute/path/to/freemkv [--no-start]" >&2
  exit 2
fi
binary=$1
if [[ $binary != /* || ! -x $binary || (${2:-} != '' && ${2:-} != --no-start) ]]; then
  echo 'Provide an absolute executable path and optionally --no-start.' >&2
  exit 2
fi
"$binary" server --help >/dev/null
label=io.freemkv.library
agent_dir="$HOME/Library/LaunchAgents"
plist="$agent_dir/$label.plist"
mkdir -p "$agent_dir"
tmp=$(mktemp "$agent_dir/.library-install.XXXXXX")
trap 'rm -f "$tmp"' EXIT
if [[ -f $plist ]]; then
  cp "$plist" "$tmp"
else
  data_dir="${AUTORIP_DIR:-$HOME/Library/Application Support/freemkv-library}"
  mkdir -p "$data_dir/logs"
  /usr/bin/plutil -create xml1 "$tmp"
  /usr/bin/plutil -insert Label -string "$label" "$tmp"
  /usr/bin/plutil -insert ProgramArguments -json '[]' "$tmp"
  /usr/bin/plutil -insert ProgramArguments.0 -string "$binary" "$tmp"
  /usr/bin/plutil -insert ProgramArguments.1 -string server "$tmp"
  /usr/bin/plutil -insert ProgramArguments.2 -string serve "$tmp"
  /usr/bin/plutil -insert EnvironmentVariables -json '{}' "$tmp"
  /usr/bin/plutil -insert EnvironmentVariables.AUTORIP_DIR -string "$data_dir" "$tmp"
  /usr/bin/plutil -insert EnvironmentVariables.PORT -string "${PORT:-8080}" "$tmp"
  /usr/bin/plutil -insert EnvironmentVariables.PATH -string '/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin' "$tmp"
  /usr/bin/plutil -insert WorkingDirectory -string "$data_dir" "$tmp"
  /usr/bin/plutil -insert StandardOutPath -string "$data_dir/logs/launchd.stdout.log" "$tmp"
  /usr/bin/plutil -insert StandardErrorPath -string "$data_dir/logs/launchd.stderr.log" "$tmp"
  /usr/bin/plutil -insert RunAtLoad -bool YES "$tmp"
  /usr/bin/plutil -insert KeepAlive -bool YES "$tmp"
  /usr/bin/plutil -insert ThrottleInterval -integer 10 "$tmp"
fi
# Rebuild the argument array: plutil's indexed replacement can insert rather
# than replace on some macOS versions, duplicating argv[0] on an update.
/usr/bin/plutil -remove ProgramArguments "$tmp"
/usr/bin/plutil -insert ProgramArguments -json '[]' "$tmp"
/usr/bin/plutil -insert ProgramArguments.0 -string "$binary" "$tmp"
/usr/bin/plutil -insert ProgramArguments.1 -string server "$tmp"
/usr/bin/plutil -insert ProgramArguments.2 -string serve "$tmp"
# The default launchd service policy throttles network file copies. Use the
# same resource policy as an app for this user-requested rip/mux/move service.
/usr/bin/plutil -remove ProcessType "$tmp" 2>/dev/null || true
/usr/bin/plutil -insert ProcessType -string Interactive "$tmp"
/usr/bin/plutil -lint "$tmp" >/dev/null
chmod 600 "$tmp"
if [[ -f $plist ]]; then cp -p "$plist" "$plist.previous"; fi
mv "$tmp" "$plist"
if [[ ${2:-} == --no-start ]]; then
  echo "Installed $plist (not started)."
elif launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then
  echo 'Updated service configuration. The running Library was left untouched.'
  echo 'After its jobs finish, log out and back in to apply the new configuration.'
else
  launchctl bootstrap "gui/$(id -u)" "$plist"
  echo 'Library started. Open http://localhost:8080/drives (or your configured PORT).'
fi
