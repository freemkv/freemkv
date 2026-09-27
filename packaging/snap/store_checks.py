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
    r"\(NEEDS REVIEW\)\s+(?P<id>[\w-]+(?::[\w-]+){2,})"
    r"(?:\s+" + re.escape(GRANT_REASON) + r")?\.?"
)
# craft-cli prints these after an error's message; they end the issue list.
ERROR_TRAILERS = (
    "Detailed information:",
    "Recommended resolution:",
    "For more information, check out:",
    "Full execution log:",
)
# After a trailer, a (TAG)/[TAG] marker means another finding, not trailer text.
TAG_MARKER = re.compile(r"[(\[][A-Z][A-Z ]*[)\]]")

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
    issues, trailer = [], False
    for line in log.split(ISSUES_HEADER, 1)[1].splitlines():
        if not line.strip():
            continue
        if trailer or line.startswith(ERROR_TRAILERS):
            trailer = True
            if ISSUES_HEADER in line or looks_like_a_finding(line):
                return None
            continue
        if not line.startswith("- "):
            return None
        issues.append(line[2:].strip())
    return issues


def looks_like_a_finding(line):
    """An issue bullet, an uppercase (TAG) or [TAG], or the word 'rejected'."""
    return bool(
        TAG_MARKER.search(line)
        or re.search(r"\brejected\b", line, re.IGNORECASE)
        or line.lstrip().startswith("- ")
    )


def is_known_grant_issue(issue):
    """Exactly `(NEEDS REVIEW) <known grant id>`, optionally with its known reason."""
    m = GRANT_ISSUE.fullmatch(issue.strip())
    return m is not None and m["id"] in STORE_GRANT_IDS


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
        with open(argv[3]) as f:
            print(classify_upload(code, f.read()))
        return 0
    print(f"usage: {argv[0]} review EXIT_CODE REPORT.json | upload EXIT_CODE LOG", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
