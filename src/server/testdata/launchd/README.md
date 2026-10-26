Sanitized full-record `launchctl print` snapshots measured on macOS 27.0.1
(build 26A434) with owned disposable jobs on 2026-10-26. Labels and worker/plist
paths are replaced with fixture values. No application jobs were queried or
changed. `stale-disk-true` was captured after editing KeepAlive on disk without
reloading the loaded false definition. Conditional before/after use loaded
SuccessfulExit=false; successful-true is the inverse control before first exit.
These fixtures describe empirical diagnostic syntax, not a stable Apple API.
