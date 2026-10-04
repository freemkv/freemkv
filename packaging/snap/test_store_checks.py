import unittest

import store_checks as sc

OK_SECTION = {"error": {}, "warn": {}, "info": {}}
PLUG, SLOT = sc.KNOWN_GRANTS


def report(errors=None, warns=None):
    return {"snap.v2_declaration": {"error": errors or {}, "warn": warns or {}, "info": {}},
            "snap.v2_lint": dict(OK_SECTION)}


class Review(unittest.TestCase):
    def test_exactly_two_known_grants(self):
        self.assertEqual(len(sc.KNOWN_GRANTS), 2)

    def test_known_grants_pass(self):
        lines, bad = sc.review(report({PLUG: {"text": "human review"}, SLOT: {"text": "x"}}))
        self.assertEqual(bad, [])
        self.assertTrue(all(line.startswith("ALLOWED") for line in lines))

    def test_other_declaration_error_fails(self):
        other = "declaration-snap-v2:plugs_connection:raw-usb:raw-usb"
        _, bad = sc.review(report({PLUG: {}, other: {"text": "human review"}}))
        self.assertEqual(bad, [other])

    def test_prefix_of_a_grant_is_not_allowed(self):
        _, bad = sc.review(report({PLUG + "-extra": {}}))
        self.assertEqual(len(bad), 1)

    def test_warnings_do_not_fail(self):
        _, bad = sc.review(report(warns={"lint-snap-v2:foo": {"text": "w"}}))
        self.assertEqual(bad, [])

    def test_unexpected_format_raises(self):
        for bad_report in ({}, [], {"s": "text"}, {"s": {"info": {}}}):
            with self.assertRaises(ValueError):
                sc.review(bad_report)


class ReviewExit(unittest.TestCase):
    def test_exit_matches_report(self):
        self.assertEqual(sc.expected_review_exit(report({PLUG: {}})), 2)
        self.assertEqual(sc.expected_review_exit(report(warns={"w": {}})), 3)
        self.assertEqual(sc.expected_review_exit(report()), 0)

    def _run(self, code, rep):
        import json, os, tempfile
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump(rep, f)
        try:
            return sc.main(["store_checks.py", "review", str(code), f.name])
        finally:
            os.unlink(f.name)

    def test_nonzero_exit_with_clean_report_fails(self):
        self.assertEqual(self._run(1, report()), 1)
        self.assertEqual(self._run(2, report()), 1)

    def test_exit_3_with_a_warnings_only_report_passes(self):
        self.assertEqual(self._run(3, report(warns={"lint-snap-v2:foo": {"text": "w"}})), 0)

    def test_non_numeric_exit_code_fails(self):
        self.assertEqual(self._run("x", report()), 2)

    def test_expected_exit_with_known_grants_passes(self):
        self.assertEqual(self._run(2, report({PLUG: {}, SLOT: {}})), 0)
        self.assertEqual(self._run(0, report()), 0)


BARE_PLUG, BARE_SLOT = (g.split(":", 1)[1] for g in sc.KNOWN_GRANTS)
START = (
    "Starting snapcraft, version 9.1.3\n"
    "Logging execution to '/root/snapcraft-1.log'\n"
    "Unsquashing snap file 'freemkv-amd64.snap'.\n"
)
PRE = START + "Uploading...\nStatus: processing\nStatus: will need manual review\n"
HDR = sc.ISSUES_HEADER + "\n"
LOG = "Full execution log: '/root/snapcraft-1.log'\n"
DOCS = "For more information, check out: https://snapcraft.io/docs/store-review\n"


def item(grant, reason=True, stop=""):
    return f"- (NEEDS REVIEW) {grant}" + (f" {sc.GRANT_REASON}" if reason else "") + stop + "\n"


HELD = PRE + HDR + item(PLUG) + item(SLOT) + LOG


class UploadHeld(unittest.TestCase):
    def test_legit_held_variants(self):
        cases = {
            "both grants": HELD,
            "plug only": PRE + HDR + item(PLUG) + LOG,
            "slot only, no trailer": PRE + HDR + item(SLOT),
            "unprefixed ids": PRE + HDR + item(BARE_PLUG) + item(BARE_SLOT) + LOG,
            "no reason": PRE + HDR + item(PLUG, reason=False) + LOG,
            "full stop after id": PRE + HDR + item(PLUG, reason=False, stop=".") + LOG,
            "full stop after reason": PRE + HDR + item(SLOT, stop=".") + LOG,
            "docs link and log": PRE + HDR + item(PLUG) + DOCS + LOG,
            "blank lines": PRE + "\n" + HDR + "\n" + item(PLUG) + "\n" + LOG,
            "CRLF": (PRE + HDR + item(PLUG) + item(SLOT) + LOG).replace("\n", "\r\n"),
            "held status only": "Status: will need manual review\n" + HDR + item(PLUG),
            "bare reasons": PRE + HDR + f"- {sc.GRANT_REASON}\n" * 2 + LOG,
            "bare reason, full stop": PRE + HDR + f"- {sc.GRANT_REASON}.\n" + LOG,
            "bare reason and id": PRE + HDR + item(PLUG) + f"- {sc.GRANT_REASON}\n" + LOG,
        }
        for name, log in cases.items():
            self.assertEqual(sc.classify_upload(1, log, "beta"), "held", name)


class RealUploads(unittest.TestCase):
    # Verbatim from the first store upload (revision 1, 1.7.7, snapcraft 9.1.3).
    REVISION_1 = (
        "Starting snapcraft, version 9.1.3\n"
        "Logging execution to '/tmp/snapcraft/log/snapcraft-20260930-011135.726998.log'\n"
        "Unsquashing snap file 'freemkv-amd64.snap'.\n"
        "Uploading... (--->)\n"
        "Uploading... (<---)\n"
        + "Status: processing\n" * 9
        + "Status: will need manual review\n"
        "Issues while processing snap:\n"
        "- human review required due to 'deny-connection' constraint (interface attributes)\n"
        "- human review required due to 'deny-connection' constraint (interface attributes)\n"
        "Full execution log: '/tmp/snapcraft/log/snapcraft-20260930-011135.726998.log'\n"
    )

    def test_revision_1_is_held(self):
        self.assertEqual(sc.classify_upload(1, self.REVISION_1, "stable"), "held")

    def test_spinner_must_be_an_arrow(self):
        log = self.REVISION_1.replace("Uploading... (--->)", "Uploading... (rejected)")
        self.assertEqual(sc.classify_upload(1, log, "stable"), "failed")


class UploadAdversarial(unittest.TestCase):
    CASES = {
        # before the header
        "error line before header": "Error: something\n" + HDR + item(PLUG),
        "traceback before header": "Traceback (most recent call last):\n" + HDR + item(PLUG),
        "unknown progress text": "Uploading 12%\n" + HDR + item(PLUG),
        "status with trailing text": "Status: processing (REJECTED)\n" + HDR + item(PLUG),
        "bullet before header": "- (REJECTED) raw-usb\n" + HDR + item(PLUG),
        "indented header": "  " + HDR + item(PLUG),
        "header with suffix": sc.ISSUES_HEADER + " (1 of 2)\n" + item(PLUG),
        "header lowercase": HDR.lower() + item(PLUG),
        "no header": PRE + item(PLUG),
        "header only": PRE + HDR + LOG,
        "no status before header": START + HDR + item(PLUG),
        "rejected status before header": PRE + "Status: rejected\n" + HDR + item(PLUG),
        "processing error status last": PRE + "Status: error while processing\n" + HDR + item(PLUG),
        "startup line inside issues": PRE + HDR + item(PLUG) + "Starting snapcraft, version 9.1.3\n",
        "startup line after trailer": PRE + HDR + item(PLUG) + LOG + "Logging execution to '/x'\n",
        # the issue lines
        "no status tag": HDR + f"- {PLUG}\n",
        "lowercase tag": HDR + f"- (needs review) {PLUG}\n",
        "rejected tag": HDR + f"- (REJECTED) {PLUG}\n",
        "bracketed tag": HDR + f"- [NEEDS REVIEW] {PLUG}\n",
        "two tags": HDR + f"- (NEEDS REVIEW) (REJECTED) {PLUG}\n",
        "unknown grant": HDR + "- (NEEDS REVIEW) declaration-snap-v2:plugs_connection:raw-usb:raw-usb\n",
        "grant then rejected": HDR + f"- (NEEDS REVIEW) {PLUG}; (REJECTED) other\n",
        "grant suffix -evil": HDR + f"- (NEEDS REVIEW) {PLUG}-evil\n",
        "bare grant suffix -evil": HDR + f"- (NEEDS REVIEW) {BARE_PLUG}-evil\n",
        "trailing extra text": HDR + f"- (NEEDS REVIEW) {PLUG}, also raw-usb denied\n",
        "partial reason": HDR + f"- (NEEDS REVIEW) {PLUG} human review required\n",
        "reason then more": HDR + item(PLUG, stop="; also raw-usb"),
        "two full stops": HDR + item(PLUG, reason=False, stop=".."),
        "asterisk bullet": HDR + f"* (NEEDS REVIEW) {PLUG}\n",
        "unicode bullet": HDR + f"\u2022 (NEEDS REVIEW) {PLUG}\n",
        "indented bullet": HDR + f"  - (NEEDS REVIEW) {PLUG}\n",
        "double space": HDR + f"-  (NEEDS REVIEW) {PLUG}\n",
        "tab separator": HDR + f"- (NEEDS REVIEW)\t{PLUG}\n",
        "indented continuation": HDR + item(PLUG) + "  plugs_installation:block-devices\n",
        "unknown issue after a known one": HDR + item(PLUG) + "- (REJECTED) raw-usb\n",
        # after the issues
        "detailed information": HDR + item(PLUG) + "Detailed information: x\n",
        "recommended resolution": HDR + item(PLUG) + "Recommended resolution: y\n",
        "bullet after trailer": HDR + item(PLUG) + LOG + "- (REJECTED) raw-usb\n",
        "second issues block": HDR + item(PLUG) + LOG + HDR + "- (REJECTED) raw-usb\n",
        "text inside trailer": HDR + item(PLUG) + "Full execution log: '/x' (REJECTED)\n",
        "http docs link": HDR + item(PLUG) + "For more information, check out: http://x\n",
        "error after trailer": HDR + item(PLUG) + LOG + "Error: upload rejected\n",
        "grant after trailer": HDR + item(PLUG) + LOG + item(SLOT),
        "three bare reasons": HDR + f"- {sc.GRANT_REASON}\n" * 3,
        "partial bare reason": HDR + "- human review required\n",
        "bare reason then more": HDR + f"- {sc.GRANT_REASON}; also raw-usb\n",
        "other bare reason": HDR + "- human review required due to 'deny-installation' constraint\n",
    }

    def test_every_adversarial_case_fails(self):
        self.assertGreaterEqual(len(self.CASES), 36)
        for name, log in self.CASES.items():
            self.assertEqual(sc.classify_upload(1, log, "beta"), "failed", name)
            # With a valid preamble, so each case fails for its own defect.
            if log.startswith(HDR):
                self.assertEqual(sc.classify_upload(1, PRE + log, "beta"), "failed", name)

    def test_held_needs_exit_code_one(self):
        for code in (2, 3, 127, -1):
            self.assertEqual(sc.classify_upload(code, HELD, "beta"), "failed", code)


class UploadSuccess(unittest.TestCase):
    OK = START + "Uploading...\nStatus: processing\nStatus: ready to release!\n"
    REV = "Revision 12 created for 'freemkv' and released to 'beta'\n"

    def test_released(self):
        self.assertEqual(sc.classify_upload(0, self.OK + self.REV, "beta"), "released")
        self.assertEqual(sc.classify_upload(0, (self.OK + self.REV).replace("\n", "\r\n"), "beta"), "released")

    def test_held_status_is_not_released(self):
        log = START + "Status: will need manual review\n" + self.REV
        self.assertEqual(sc.classify_upload(0, log, "beta"), "held-unreported")

    def test_revision_must_name_the_requested_channel(self):
        self.assertEqual(sc.classify_upload(0, self.OK + self.REV, "edge"), "failed")
        rev = "Revision 12 created for 'freemkv' and released to 'stable'\n"
        self.assertEqual(sc.classify_upload(0, self.OK + rev, "stable"), "released")

    def test_unknown_channel_argument_fails(self):
        self.assertEqual(sc.classify_upload(0, self.OK + self.REV, "beta,edge"), "failed")

    def test_success_cases_that_fail(self):
        cases = {
            "no revision line": self.OK,
            "revision not a whole line": self.OK + "x " + self.REV,
            "revision with suffix": self.OK + self.REV.rstrip("\n") + " (REJECTED)\n",
            "other snap": self.OK + "Revision 12 created for 'other'\n",
            "two revision lines": self.OK + self.REV + self.REV,
            "issues header": self.OK + self.REV + HDR + item(PLUG),
            "unknown line": self.OK + "Warning: odd\n" + self.REV,
            "no status": self.REV,
            "final status not ready": "Status: processing\n" + self.REV,
            "unknown channel": self.OK + "Revision 12 created for 'freemkv' and released to 'x'\n",
            "no released-to clause": self.OK + "Revision 12 created for 'freemkv'\n",
            "revision not last": self.OK + self.REV + "Status: processing\n",
            "startup line after revision": self.OK + self.REV + "Starting snapcraft, version 9.1.3\n",
            "startup line with suffix": "Starting snapcraft, version 9.1.3 (REJECTED)\n" + self.OK + self.REV,
            "unsquashing without full stop": "Unsquashing snap file 'x.snap'\n" + self.OK + self.REV,
        }
        for name, log in cases.items():
            self.assertEqual(sc.classify_upload(0, log, "beta"), "failed", name)


class UploadMain(unittest.TestCase):
    def test_non_numeric_exit_code_is_a_numbered_failure(self):
        self.assertEqual(sc.main(["store_checks.py", "upload", "oops", "beta", "/dev/null"]), 2)

    def test_unreadable_log_is_a_numbered_failure(self):
        self.assertEqual(sc.main(["store_checks.py", "upload", "1", "beta", "/nonexistent/upload.log"]), 2)

    def test_undecodable_log_is_a_numbered_failure(self):
        import os, tempfile
        with tempfile.NamedTemporaryFile("wb", delete=False) as f:
            f.write(b"\xff\xfe\xfa")
        try:
            self.assertEqual(sc.main(["store_checks.py", "upload", "1", "beta", f.name]), 2)
        finally:
            os.unlink(f.name)


if __name__ == "__main__":
    unittest.main()


class UploadTransient(unittest.TestCase):
    # The qa upload of 2026-10-04, verbatim.
    DROPPED = (
        START
        + "Uploading... (--->)\nUploading... (<---)\n"
        + "('Connection aborted.', RemoteDisconnected('Remote end closed connection without response'))\n"
        + "Full execution log: 'snapcraft.log'\n"
    )

    def test_a_dropped_connection_is_transient(self):
        self.assertEqual(sc.classify_upload(1, self.DROPPED, "stable"), "transient")
        timeout = START + "Uploading...\nHTTPSConnectionPool(host='x'): Read timed out. (read timeout=60)\n"
        self.assertEqual(sc.classify_upload(1, timeout, "stable"), "transient")

    def test_anything_else_beside_it_is_a_failure(self):
        self.assertEqual(sc.classify_upload(1, self.DROPPED + "Status: processing\n", "stable"), "failed")
        self.assertEqual(sc.classify_upload(1, self.DROPPED + "Error: denied\n", "stable"), "failed")
        self.assertEqual(sc.classify_upload(1, START + "Error: denied\n", "stable"), "failed")

    def test_a_zero_exit_is_never_transient(self):
        self.assertNotEqual(sc.classify_upload(0, self.DROPPED, "stable"), "transient")


class Login(unittest.TestCase):
    from datetime import datetime, timezone
    NOW = datetime(2026, 10, 4, tzinfo=timezone.utc)
    WHOAMI = (
        "email: dev@example.com\nusername: freemkv\nid: abc\n"
        # What the project's credentials report.
        "permissions: package_access, package_push, package_update, package_release\n"
        "channels: no restrictions\nexpires: 2027-03-01T00:00:00.000Z\n"
    )

    def test_good_credentials_pass(self):
        self.assertEqual(sc.check_login(self.WHOAMI, 30, self.NOW), [])

    def test_expiring_soon_fails(self):
        soon = self.WHOAMI.replace("2027-03-01", "2026-10-20")
        self.assertEqual(len(sc.check_login(soon, 30, self.NOW)), 1)

    def test_missing_permission_fails(self):
        no_release = self.WHOAMI.replace(", package_release", "")
        self.assertEqual(sc.check_login(no_release, 30, self.NOW), ["store credentials lack package_release"])

    def test_no_expiry_fails(self):
        no_expiry = "\n".join(l for l in self.WHOAMI.splitlines() if not l.startswith("expires"))
        self.assertEqual(len(sc.check_login(no_expiry, 30, self.NOW)), 1)
