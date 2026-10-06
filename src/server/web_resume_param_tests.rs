use super::{ResumeMode, parse_resume_param};

// ?resume=no selects Wipe, DELETING an existing staging dir before a
// fresh sweep — the only unauthenticated param that can destroy a rip.
// Had no test; a mutation flipped the key comparison and stayed green.
#[test]
fn resume_param_maps_only_the_documented_values() {
    // The destructive one. Every spelling of it.
    for q in ["resume=no", "resume=false", "resume=0"] {
        assert_eq!(
            parse_resume_param(q),
            ResumeMode::Wipe,
            "{q} must select Wipe"
        );
    }
    for q in ["resume=yes", "resume=true", "resume=1"] {
        assert_eq!(
            parse_resume_param(q),
            ResumeMode::Require,
            "{q} must select Require"
        );
    }

    // Anything else is Default — never the destructive mode. An
    // unrecognised value must not be read as "no".
    for q in [
        "resume=maybe",
        "resume=",
        "resume",
        "foo=bar",
        "",
        "RESUME=no", // the key match is case-sensitive
        "resume=NO", // ...and so is the value
    ] {
        assert_eq!(
            parse_resume_param(q),
            ResumeMode::Default,
            "{q:?} must fall through to Default, never Wipe"
        );
    }
}

// The scan must match on the KEY, not the first value that looks like
// one — with == flipped to != a URL like ?title=no&resume=yes would wipe
// the staging dir the operator asked to keep.
#[test]
fn resume_param_reads_the_resume_key_not_a_neighbouring_one() {
    assert_eq!(
        parse_resume_param("title=no&resume=yes"),
        ResumeMode::Require
    );
    assert_eq!(parse_resume_param("a=1&b=2&resume=no"), ResumeMode::Wipe);
    // A key that merely contains "resume" is not the resume key.
    assert_eq!(parse_resume_param("presume=no"), ResumeMode::Default);
    assert_eq!(parse_resume_param("resumed=no"), ResumeMode::Default);
    // First match wins and stops the scan.
    assert_eq!(
        parse_resume_param("resume=yes&resume=no"),
        ResumeMode::Require
    );
}
