"""Judge review-tools reports and Snap Store upload results for snap.yml.

Only the two store grants freemkv is known to need (packaging/snap/README.md)
are tolerated; every other finding, and any output in a format this script
does not recognise, is a failure.
"""

import json
import re
import sys

# review-tools check ids for the two grants.
KNOWN_GRANTS = (
    "declaration-snap-v2:plugs_connection:optical-write:optical-drive",
    "declaration-snap-v2:slots_connection:freemkv-dbus:dbus",
)
# A store upload error may name a grant with or without the review-tools prefix.
STORE_GRANT_IDS = frozenset(KNOWN_GRANTS) | {g.split(":", 1)[1] for g in KNOWN_GRANTS}
# review-tools' explanation for both grants; the only text allowed after the id.
GRANT_REASON = "human review required due to 'deny-connection' constraint (interface attributes)"
# The whole shape of a held issue: the tag, one check id, optionally the known
# reason, and at most a closing full stop.
GRANT_ISSUE = re.compile(
    r"\(NEEDS REVIEW\) (?P<id>[a-z0-9_-]+(?::[a-z0-9_-]+){2,})"
    r"(?: " + re.escape(GRANT_REASON) + r")?\.?"
)
# What `snapcraft upload --verbosity=verbose` (snapcraft 9) prints, whole lines
# only: the progress-bar caption, the status poll (store/client.py
# _HUMAN_STATUS, or a raw status code), the result, and craft-cli's error
# trailers. Anything else makes the upload "failed".
PROGRESS = re.compile(
    r"Uploading\.\.\."
    r"|Status: (?:processing|ready to release!|will need manual review"
    r"|error while processing delta|error while processing|[a-z_]+)"
)
HELD_STATUS = "Status: will need manual review"
ISSUES_HEADER = "Issues while processing snap:"
REVISION = re.compile(
    r"Revision \d+ created for 'freemkv'"
    r"(?: and released to '(?:edge|beta|candidate|stable)')?"
)
TRAILER = re.compile(
    r"Full execution log: '[^'\n]+'|For more information, check out: https://\S+"
)
READY_STATUS = "Status: ready to release!"

def expected_review_exit(report):
    """snap-review's exit code for a report: 2 with errors, else 3 with warnings, else 0."""
    if any(r["error"] for r in report.values()):
        return 2
    return 3 if any(r["warn"] for r in report.values()) else 0


def review(report):
    """Return (lines, failures) for a review-tools --json report."""
    if not isinstance(report, dict) or not report:
        raise ValueError("review-tools report is not a non-empty JSON object")
    lines, failures = [], []
    for section, results in report.items():
        if not isinstance(results, dict) or not {"error", "warn"} <= results.keys():
            raise ValueError(f"review-tools section {section!r} has no error/warn keys")
        for level in ("error", "warn"):
            for check, detail in (results[level] or {}).items():
                tag = "ALLOWED" if check in KNOWN_GRANTS else level.upper()
                text = detail.get("text", "") if isinstance(detail, dict) else ""
                lines.append(f"{tag:8} {section} {check}: {text}")
                if tag != "ALLOWED" and level == "error":
                    failures.append(check)
    return lines, failures


def is_known_grant_issue(issue):
    """Exactly `(NEEDS REVIEW) <known grant id>`, optionally with its known reason."""
    m = GRANT_ISSUE.fullmatch(issue)
    return m is not None and m["id"] in STORE_GRANT_IDS


def _released_or_held(lines):
    """Exit 0: progress lines and one revision line; the final status decides."""
    statuses, revisions = [], 0
    for line in lines:
        if not line.strip():
            continue
        if PROGRESS.fullmatch(line):
            if line.startswith("Status: "):
                statuses.append(line)
        elif REVISION.fullmatch(line):
            revisions += 1
        else:
            return "failed"
    if revisions != 1:
        return "failed"
    last = statuses[-1] if statuses else None
    return {READY_STATUS: "released", HELD_STATUS: "held"}.get(last, "failed")


def _held_by_known_grants(lines):
    """Exit 1: progress, the header, known-grant issues, then craft-cli trailers."""
    state, issues = "progress", 0
    for line in lines:
        if not line.strip():
            continue
        if state == "progress":
            if line == ISSUES_HEADER:
                state = "issues"
            elif not PROGRESS.fullmatch(line):
                return "failed"
        elif state == "issues" and line.startswith("- ") and is_known_grant_issue(line[2:]):
            issues += 1
        elif issues and TRAILER.fullmatch(line):
            state = "trailer"
        else:
            return "failed"
    return "held" if issues else "failed"


def classify_upload(exit_code, log):
    """'released', 'held' (waiting for the known store grants) or 'failed'."""
    lines = log.splitlines()
    if exit_code == 0:
        if ISSUES_HEADER in log:
            return "failed"
        return _released_or_held(lines)
    if exit_code == 1:
        return _held_by_known_grants(lines)
    return "failed"


def main(argv):
    if len(argv) == 4 and argv[1] == "review":
        try:
            with open(argv[3]) as f:
                report = json.load(f)
            lines, failures = review(report)
            code = int(argv[2])
        except (ValueError, json.JSONDecodeError) as e:
            print(f"::error::unexpected review-tools output or exit code: {e}")
            return 2
        print("\n".join(lines))
        want = expected_review_exit(report)
        if code != want:
            print(f"::error::snap-review exited {argv[2]}, but its report implies {want}")
            return 1
        return 1 if failures else 0
    if len(argv) == 4 and argv[1] == "upload":
        try:
            code = int(argv[2])
        except ValueError:
            print(f"::error::snapcraft exit code {argv[2]!r} is not a number")
            return 2
        try:
            with open(argv[3], encoding="utf-8") as f:
                log = f.read()
        except (OSError, UnicodeDecodeError) as e:
            print(f"::error::cannot read the snapcraft upload log: {e}")
            return 2
        print(classify_upload(code, log))
        return 0
    print(f"usage: {argv[0]} review EXIT_CODE REPORT.json | upload EXIT_CODE LOG", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
