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


class Upload(unittest.TestCase):
    RELEASED = "Status: ready to release!\nRevision 12 created for 'freemkv' and released to 'beta'\n"

    def test_released(self):
        self.assertEqual(sc.classify_upload(0, self.RELEASED), "released")

    def test_held_without_errors_is_not_released(self):
        log = "Status: will need manual review\nRevision 12 created for 'freemkv' and released to 'beta'\n"
        self.assertEqual(sc.classify_upload(0, log), "held")

    def test_success_in_an_unknown_format_fails(self):
        self.assertEqual(sc.classify_upload(0, "all good\n"), "failed")

    def test_only_known_grant_issues_are_held(self):
        log = (f"{sc.ISSUES_HEADER}\n- (NEEDS REVIEW) {PLUG} human review required\n"
               f"- (NEEDS REVIEW) {SLOT} human review required\n")
        self.assertEqual(sc.classify_upload(1, log), "held")

    def test_any_other_needs_review_fails(self):
        log = (f"{sc.ISSUES_HEADER}\n- (NEEDS REVIEW) {PLUG} human review required\n"
               "- (NEEDS REVIEW) declaration-snap-v2:plugs_installation:block-devices\n")
        self.assertEqual(sc.classify_upload(1, log), "failed")

    def test_manual_review_words_alone_do_not_hold(self):
        self.assertEqual(sc.classify_upload(1, "needs manual review\nhuman review\n"), "failed")

    def test_other_errors_fail(self):
        self.assertEqual(sc.classify_upload(1, "Invalid credentials\n"), "failed")


if __name__ == "__main__":
    unittest.main()
