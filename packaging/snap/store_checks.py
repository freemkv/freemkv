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
# A whole review check id: colon-separated words, at least three parts.
CHECK_ID = re.compile(r"(?<![\w:.-])[\w.-]+(?::[\w.-]+){2,}(?![\w:.-])")
STATUS_TAG = re.compile(r"\(([A-Z][A-Z ]*)\)")
LOG_TRAILER = "Full execution log:"

HELD_STATUS = "will need manual review"
ISSUES_HEADER = "Issues while processing snap:"
CREATED = re.compile(r"Revision \d+ created for 'freemkv'")


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


def upload_issues(log):
    """The issue lines under snapcraft's 'Issues while processing snap:' header.

    None if anything there is not a plain top-level `- ` item, such as an
    indented continuation line: an unfamiliar shape is never trusted.
    """
    if ISSUES_HEADER not in log:
        return None
    issues = []
    for line in log.split(ISSUES_HEADER, 1)[1].splitlines():
        if not line.strip():
            continue
        if line.startswith(LOG_TRAILER):
            break
        if not line.startswith("- "):
            return None
        issues.append(line[2:].strip())
    return issues


def is_known_grant_issue(issue):
    """One NEEDS REVIEW finding naming exactly one known grant, as a whole token."""
    if ";" in issue or any(t != "NEEDS REVIEW" for t in STATUS_TAG.findall(issue)):
        return False
    ids = CHECK_ID.findall(issue)
    return len(ids) == 1 and ids[0] in STORE_GRANT_IDS


def classify_upload(exit_code, log):
    """'released', 'held' (waiting for the known store grants) or 'failed'."""
    if exit_code == 0:
        if not CREATED.search(log):
            return "failed"
        return "held" if HELD_STATUS in log else "released"
    issues = upload_issues(log)
    if issues and all(is_known_grant_issue(i) for i in issues):
        return "held"
    return "failed"


def main(argv):
    if len(argv) == 4 and argv[1] == "review":
        try:
            with open(argv[3]) as f:
                report = json.load(f)
            lines, failures = review(report)
        except (ValueError, json.JSONDecodeError) as e:
            print(f"::error::unexpected review-tools output: {e}")
            return 2
        print("\n".join(lines))
        want = expected_review_exit(report)
        if int(argv[2]) != want:
            print(f"::error::snap-review exited {argv[2]}, but its report implies {want}")
            return 1
        return 1 if failures else 0
    if len(argv) == 4 and argv[1] == "upload":
        with open(argv[3]) as f:
            print(classify_upload(int(argv[2]), f.read()))
        return 0
    print(f"usage: {argv[0]} review EXIT_CODE REPORT.json | upload EXIT_CODE LOG", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
