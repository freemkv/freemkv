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
    r"Uploading\.\.\.(?: \((?:-+>|<-+)\))?"
    r"|Status: (?:processing|ready to release!|will need manual review"
    r"|error while processing delta|error while processing|[a-z_]+)"
)
# Printed before any progress at --verbosity=verbose: craft-cli's greeting and
# log path (messages.py), craft-application's greeting text, and snapcraft
# reading the snap (utils.py).
STARTUP = re.compile(
    r"Starting snapcraft, version \S+"
    r"|Logging execution to '[^'\n]+'"
    r"|Unsquashing snap file '[^'\n]+'\."
)
CHANNELS = ("edge", "beta", "candidate", "stable")
HELD_STATUS = "Status: will need manual review"
ISSUES_HEADER = "Issues while processing snap:"
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


def is_bare_grant_reason(issue):
    """The known reason alone, as the store printed it for revision 1 (snapcraft
    9.1.3): one line per grant, no tag and no check id."""
    return issue in (GRANT_REASON, GRANT_REASON + ".")


def _preamble_line(line):
    """A startup or progress line, allowed before the result or the issue list."""
    return bool(STARTUP.fullmatch(line) or PROGRESS.fullmatch(line))


def _last_status(lines):
    statuses = [line for line in lines if line.startswith("Status: ")]
    return statuses[-1] if statuses else None


def _released_or_held(lines, channel):
    """Exit 0: preamble, then the revision line for `channel` as the last line."""
    content = [line for line in lines if line.strip()]
    if not content:
        return "failed"
    revision = re.compile(
        r"Revision \d+ created for 'freemkv' and released to '" + re.escape(channel) + "'"
    )
    *preamble, last = content
    if not revision.fullmatch(last) or not all(_preamble_line(line) for line in preamble):
        return "failed"
    return {READY_STATUS: "released", HELD_STATUS: "held-unreported"}.get(
        _last_status(preamble), "failed"
    )


def _held_by_known_grants(lines):
    """Exit 1: preamble ending in a manual-review status, the header,
    known-grant issues, then craft-cli trailers."""
    state, issues, bare, preamble = "preamble", 0, 0, []
    for line in lines:
        if not line.strip():
            continue
        if state == "preamble":
            if line == ISSUES_HEADER:
                if _last_status(preamble) != HELD_STATUS:
                    return "failed"
                state = "issues"
            elif _preamble_line(line):
                preamble.append(line)
            else:
                return "failed"
        elif state == "issues" and line.startswith("- ") and is_known_grant_issue(line[2:]):
            issues += 1
        # Bare reasons name no grant, so allow no more of them than known grants.
        elif (state == "issues" and line.startswith("- ") and is_bare_grant_reason(line[2:])
              and bare < len(KNOWN_GRANTS)):
            issues += 1
            bare += 1
        elif issues and TRAILER.fullmatch(line):
            state = "trailer"
        else:
            return "failed"
    return "held" if issues else "failed"


def classify_upload(exit_code, log, channel):
    """'released', 'held' (the known store grants), 'held-unreported' (held,
    no reason given) or 'failed'."""
    if channel not in CHANNELS:
        return "failed"
    lines = log.splitlines()
    if exit_code == 0:
        if ISSUES_HEADER in log:
            return "failed"
        return _released_or_held(lines, channel)
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
    if len(argv) == 5 and argv[1] == "upload":
        try:
            code = int(argv[2])
        except ValueError:
            print(f"::error::snapcraft exit code {argv[2]!r} is not a number")
            return 2
        try:
            with open(argv[4], encoding="utf-8") as f:
                log = f.read()
        except (OSError, UnicodeDecodeError) as e:
            print(f"::error::cannot read the snapcraft upload log: {e}")
            return 2
        print(classify_upload(code, log, argv[3]))
        return 0
    print(f"usage: {argv[0]} review EXIT_CODE REPORT.json | upload EXIT_CODE CHANNEL LOG", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
