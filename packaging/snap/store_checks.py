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
# How each grant shows up inside a store upload error line.
STORE_GRANT_MARKERS = tuple(g.split(":", 1)[1] for g in KNOWN_GRANTS)

HELD_STATUS = "will need manual review"
ISSUES_HEADER = "Issues while processing snap:"
CREATED = re.compile(r"Revision \d+ created for 'freemkv'")


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


def upload_issues(log):
    """The `- ` lines snapcraft prints under its 'Issues while processing snap:' header."""
    if ISSUES_HEADER not in log:
        return []
    tail = log.split(ISSUES_HEADER, 1)[1].splitlines()
    return [line.strip()[2:].strip() for line in tail if line.strip().startswith("- ")]


def classify_upload(exit_code, log):
    """'released', 'held' (waiting for the known store grants) or 'failed'."""
    if exit_code == 0:
        if not CREATED.search(log):
            return "failed"
        return "held" if HELD_STATUS in log else "released"
    issues = upload_issues(log)
    if issues and all(any(m in i for m in STORE_GRANT_MARKERS) for i in issues):
        return "held"
    return "failed"


def main(argv):
    if len(argv) == 3 and argv[1] == "review":
        try:
            with open(argv[2]) as f:
                lines, failures = review(json.load(f))
        except (ValueError, json.JSONDecodeError) as e:
            print(f"::error::unexpected review-tools output: {e}")
            return 2
        print("\n".join(lines))
        return 1 if failures else 0
    if len(argv) == 4 and argv[1] == "upload":
        with open(argv[3]) as f:
            print(classify_upload(int(argv[2]), f.read()))
        return 0
    print(f"usage: {argv[0]} review REPORT.json | upload EXIT_CODE LOG", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
