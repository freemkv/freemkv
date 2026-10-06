use super::{
    PendingDiag, canon_url, drop_process_serial, is_flag_token, is_url_token, parse_logging_flags,
    same_stream_url, split_log_path, strip_language_flag, wants_help,
};

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// The one deliberate exception: a leading `-` on a NEGATIVE NUMBER is a value, not a flag,
// so `--log-level -1` reaches the range check.
#[test]
fn is_flag_token_treats_negative_numbers_as_values_not_flags() {
    assert!(is_flag_token("--raw"));
    assert!(is_flag_token("-t"));
    assert!(!is_flag_token("-1"), "a negative number is a value");
    assert!(!is_flag_token("-9"));
    assert!(!is_flag_token("disc://"), "a positional is not a flag");
    assert!(!is_flag_token(""), "an empty token is not a flag");
    // A bare '-' has nothing after the dash — not a flag.
    assert!(!is_flag_token("-"));
}

/// `is_url_token` is the schemeless-URL gate: anything carrying `://` is a
/// positional stream URL, everything else (a keydb path, a bare number) is
/// not — so a value-flag does not misread its value as a positional.
#[test]
fn is_url_matches_only_scheme_bearing_tokens() {
    assert!(is_url_token("disc://"));
    assert!(is_url_token("mkv://out.mkv"));
    assert!(is_url_token("https://keys.example/api"));
    assert!(!is_url_token("keydb.cfg"));
    assert!(!is_url_token("/path/to/out.mkv"));
    assert!(!is_url_token("3"));
}

// Just the two REQUESTS; diagnostics are a separate axis with their own tests.
fn flags(args: &[String]) -> (Option<u8>, Option<String>) {
    let (level, file, _) = parse_logging_flags(args);
    (level, file)
}

fn keys(diags: &[PendingDiag]) -> Vec<&'static str> {
    diags.iter().map(|d| d.key).collect()
}

/// The common path: no logging flags means NO subscriber is installed and
/// the terminal stays clean. If either half of that reads as "requested",
/// every plain `freemkv` run starts writing ./log.txt.
#[test]
fn no_logging_flags_requests_nothing() {
    assert_eq!(flags(&v([].as_slice())), (None, None));
    assert_eq!(
        flags(&v(&["freemkv", "iso://a.iso", "mkv://b.mkv", "-t", "2"])),
        (None, None)
    );
}

/// `--log-level` maps 1..4 and is CLAMPED, not wrapped.
#[test]
fn the_log_level_values_map_and_clamp() {
    for n in 1..=4u8 {
        assert_eq!(
            parse_logging_flags(&v(&["--log-level", &n.to_string()])).0,
            Some(n)
        );
    }
    // Above the range clamps to trace rather than being dropped.
    assert_eq!(parse_logging_flags(&v(&["--log-level", "9"])).0, Some(4));
    assert_eq!(parse_logging_flags(&v(&["--log-level", "255"])).0, Some(4));
}

// Bad input is reported and IGNORED, never clamped up to 1.
#[test]
fn a_bad_log_level_is_ignored_rather_than_guessed_at() {
    assert_eq!(parse_logging_flags(&v(&["--log-level", "0"])).0, None);
    assert_eq!(parse_logging_flags(&v(&["--log-level", "xyz"])).0, None);
    assert_eq!(parse_logging_flags(&v(&["--log-level", "-1"])).0, None);
    // Last token, no value: no panic, nothing requested.
    assert_eq!(flags(&v(&["--log-level"])), (None, None));
}

/// `--log-file` takes the following token, in either flag order, and does
/// not need `--log-level` to be present.
#[test]
fn the_log_file_flag_takes_the_next_token_in_either_order() {
    assert_eq!(
        flags(&v(&["--log-file", "/tmp/x.log"])),
        (None, Some("/tmp/x.log".to_string()))
    );
    assert_eq!(
        flags(&v(&["--log-file", "a.log", "--log-level", "2"])),
        (Some(2), Some("a.log".to_string()))
    );
    assert_eq!(
        flags(&v(&["--log-level", "2", "--log-file", "a.log"])),
        (Some(2), Some("a.log".to_string()))
    );
    // Trailing `--log-file` with no value: nothing requested, no panic.
    assert_eq!(flags(&v(&["--log-file"])), (None, None));
}

// A refused --log-file value must be REPORTED, not swallowed silently — absence of a log is
// itself a bug.
#[test]
fn a_refused_log_file_value_records_a_diagnostic() {
    // Next token is a flag: value refused, so the path is None AND a
    // complaint is recorded.
    let (_, file, diags) = parse_logging_flags(&v(&["--log-file", "--raw"]));
    assert_eq!(file, None);
    assert_eq!(keys(&diags), vec!["error.log_file_needs_value"]);

    // Last token, no value at all: same complaint.
    let (_, file, diags) = parse_logging_flags(&v(&["--log-file"]));
    assert_eq!(file, None);
    assert_eq!(keys(&diags), vec!["error.log_file_needs_value"]);

    // A real value still takes cleanly and records nothing.
    let (_, file, diags) = parse_logging_flags(&v(&["--log-file", "out.log"]));
    assert_eq!(file.as_deref(), Some("out.log"));
    assert!(diags.is_empty());
}

/// A bare filename logs into the current directory; a directory-qualified
/// one logs there. A value with no filename component is invalid and the
/// caller must report it rather than write somewhere unintended.
#[test]
fn the_log_path_splits_into_a_directory_and_a_name() {
    let (dir, name) = split_log_path("log.txt").expect("a bare name is valid");
    assert_eq!(dir, std::path::Path::new("."));
    assert_eq!(name, "log.txt");

    let (dir, name) = split_log_path("/var/log/freemkv.log").expect("a full path is valid");
    assert_eq!(dir, std::path::Path::new("/var/log"));
    assert_eq!(name, "freemkv.log");

    let (dir, name) = split_log_path("logs/a.log").expect("a relative dir is valid");
    assert_eq!(dir, std::path::Path::new("logs"));
    assert_eq!(name, "a.log");

    assert!(
        split_log_path("").is_none(),
        "an empty path has no filename"
    );
    assert!(split_log_path("/").is_none(), "a root path has no filename");
    assert!(split_log_path("..").is_none());
}

// A macOS LaunchServices `-psn_0_<n>` is not a command: dropped when it leads, so it can
// never become a bogus source URL.
#[test]
fn a_leading_macos_process_serial_is_dropped_before_dispatch() {
    assert_eq!(
        drop_process_serial(v(&["freemkv", "-psn_0_42"])),
        v(&["freemkv"])
    );
    assert_eq!(
        drop_process_serial(v(&["freemkv", "-psn_0_42", "info"])),
        v(&["freemkv", "info"])
    );
    let later = v(&["freemkv", "info", "-psn_0_42"]);
    assert_eq!(drop_process_serial(later.clone()), later);
    assert_eq!(drop_process_serial(v(&["freemkv"])), v(&["freemkv"]));
    assert_eq!(drop_process_serial(Vec::new()), Vec::<String>::new());
}

// The single-URL `info` path drops the URL from its original slot by this comparison; a
// miss passes it twice and info_cmd rejects the duplicate.
#[test]
fn same_stream_url_ignores_scheme_case_and_trailing_slashes_only() {
    assert!(same_stream_url("disc://", "disc://"));
    assert!(same_stream_url("DISC://", "disc://"));
    assert!(same_stream_url("disc://dev/sr0/", "disc://dev/sr0"));
    assert!(same_stream_url("Iso://a.iso", "iso://a.iso/"));
    // The path is case-sensitive; only the scheme folds.
    assert!(!same_stream_url("iso://A.iso", "iso://a.iso"));
    assert!(!same_stream_url("disc://dev/sr0", "disc://dev/sr1"));
    assert!(!same_stream_url("iso://a.iso", "mkv://a.iso"));
    // Schemeless tokens compare byte-for-byte only.
    assert!(same_stream_url("-v", "-v"));
    assert!(!same_stream_url("a.iso/", "a.iso"));
    assert!(!same_stream_url("-v", "disc://"));
}

#[test]
fn canon_url_lowercases_the_scheme_and_trims_trailing_slashes() {
    assert_eq!(canon_url("DISC://").as_deref(), Some("disc://"));
    assert_eq!(
        canon_url("Mkv://Out/Movie.mkv/").as_deref(),
        Some("mkv://Out/Movie.mkv")
    );
    assert_eq!(
        canon_url("disc://dev/sr0//").as_deref(),
        Some("disc://dev/sr0")
    );
    assert_eq!(canon_url("no-scheme"), None);
}

/// The value guard: `--language` must not swallow a following stream URL.
/// Without it `freemkv --language disc:// mkv://out.mkv` eats `disc://` as
/// the language and the rip degrades into a usage no-op with exit 0.
#[test]
fn the_language_flag_never_swallows_a_url_or_a_flag() {
    let (args, lang, _) = strip_language_flag(&v(&["freemkv", "--language", "de", "disc://"]));
    assert_eq!(lang.as_deref(), Some("de"));
    assert_eq!(args, v(&["freemkv", "disc://"]));

    // The short alias behaves identically.
    let (args, lang, _) = strip_language_flag(&v(&["freemkv", "--lang", "de", "disc://"]));
    assert_eq!(lang.as_deref(), Some("de"));
    assert_eq!(args, v(&["freemkv", "disc://"]));

    // A URL is not a language code — keep it positional.
    let (args, lang, _) =
        strip_language_flag(&v(&["freemkv", "--language", "disc://", "mkv://x.mkv"]));
    assert_eq!(lang, None);
    assert_eq!(args, v(&["freemkv", "disc://", "mkv://x.mkv"]));

    // Nor is a flag.
    let (args, lang, _) = strip_language_flag(&v(&["freemkv", "--language", "--verbose"]));
    assert_eq!(lang, None);
    assert_eq!(args, v(&["freemkv", "--verbose"]));

    // Last token: no value, no panic.
    let (args, lang, _) = strip_language_flag(&v(&["freemkv", "--language"]));
    assert_eq!(lang, None);
    assert_eq!(args, v(&["freemkv"]));

    // Absent entirely: the argument list is untouched.
    let original = v(&["freemkv", "iso://a.iso", "mkv://b.mkv"]);
    let (args, lang, _) = strip_language_flag(&original);
    assert_eq!(lang, None);
    assert_eq!(args, original);
}

// Every deferred diagnostic must round-trip through the real catalog (checked against
// strings::get, never PendingDiag::render, whose English fallback would make the check
// vacuous).
#[test]
fn every_deferred_startup_diagnostic_resolves_to_real_localized_text() {
    let cases = [
        (v(&["--log-level", "0"]), "error.log_level_out_of_range", ""),
        (
            v(&["--log-level", "xyz"]),
            "error.log_level_not_a_number",
            "xyz",
        ),
        (v(&["--log-level"]), "error.log_level_needs_value", ""),
    ];
    for (args, key, must_contain) in cases {
        let (_, _, diags) = parse_logging_flags(&args);
        assert_eq!(keys(&diags), vec![key], "for argv {args:?}");
        // Assert against the RAW catalog, not `PendingDiag::render`: render's
        // fallback always turns a key echo into English, so `assert_ne!(render,
        // key)` could never fail. `strings::get` echoes the key on a miss instead.
        let raw = crate::strings::get(key);
        assert_ne!(
            raw, key,
            "'{key}' has no entry in the catalog — nothing localizes it, and \
                 only `PendingDiag`'s English fallback was hiding that"
        );
        // Render still has to fill placeholders and carry the typed value.
        let text = diags[0].render();
        assert!(
            !text.contains('{'),
            "'{key}' rendered with an unsubstituted placeholder: {text}"
        );
        assert!(
            text.contains(must_contain),
            "'{key}' dropped the value the user typed: {text}"
        );
    }

    // The `--log-file` invalid-path half lives in `init_logging`, which
    // installs a process-global subscriber and so cannot be called from a
    // test. Its key is checked directly against the raw catalog instead.
    let key = "error.log_file_invalid_path";
    assert_ne!(
        crate::strings::get(key),
        key,
        "'{key}' is not in the catalog"
    );
    let text = PendingDiag::new(key, "unused fallback")
        .with("path", "/")
        .render();
    assert!(text.contains('/') && !text.contains('{'), "{text}");

    // And the language flag's own complaint, for both spellings.
    assert_ne!(
        crate::strings::get("error.language_needs_value"),
        "error.language_needs_value",
        "the language-needs-value key is not in the catalog"
    );
    for flag in ["--language", "--lang"] {
        let (_, lang, diags) = strip_language_flag(&v(&["freemkv", flag]));
        assert_eq!(lang, None);
        assert_eq!(keys(&diags), vec!["error.language_needs_value"]);
        let text = diags[0].render();
        assert!(
            text.contains(flag) && !text.contains('{'),
            "the complaint must name the spelling the user actually typed, \
                 got: {text}"
        );
    }
}

// The argv pre-pass must not print anything ITSELF — a strings::get or eprintln! here runs
// before locale is resolved.
#[test]
fn the_pre_locale_argv_pass_prints_nothing_of_its_own() {
    let src = include_str!("cli_entry.rs").replace("\r\n", "\n");
    let slice = |from: &str, to: &str| -> String {
        let a = src
            .find(from)
            .unwrap_or_else(|| panic!("anchor missing: {from}"));
        let b = src[a..]
            .find(to)
            .unwrap_or_else(|| panic!("closing anchor missing: {to}"));
        src[a..a + b].to_string()
    };
    let regions = [
        (
            "parse_logging_flags",
            slice(
                "fn parse_logging_flags(args: &[String])",
                "\n// Every word the dispatcher matches",
            ),
        ),
        (
            "strip_language_flag",
            slice(
                "fn strip_language_flag(args: &[String])",
                "\n// Print the curated fatal-error block",
            ),
        ),
    ];
    for (name, body) in regions {
        for banned in ["eprintln!", "println!", "eprint!", "print!"] {
            assert!(
                !body.contains(banned),
                "{name} contains a `{banned}`: it runs BEFORE the locale is \
                     resolved, so anything it prints is hard-coded English — \
                     and anything it localizes locks in the wrong catalog and \
                     kills --language. Record a PendingDiag instead."
            );
        }
        assert!(
            body.contains("PendingDiag::new("),
            "{name} no longer records any deferred diagnostic — the bad-input \
                 paths have gone silent"
        );
    }
}

// The `info` subcommand must speak ONE language, whatever the URL — source-pinned since
// neither the container arm nor --share is reachable from a test.
#[test]
fn the_info_surface_never_prints_english_of_its_own() {
    // CRLF-normalized: Windows CI checks the tree out with CRLF.
    let entry = include_str!("cli_entry.rs").replace("\r\n", "\n");
    let info = include_str!("info.rs").replace("\r\n", "\n");

    // Banned shape: the literal as a DIRECT macro argument (English also
    // legitimately appears as `get_or`'s fallback `english` arg) — testing
    // what's printed, not what's in the file. Collapsed/`concat!`'d against rustfmt.
    let squash = |s: &str| -> String { s.split_whitespace().collect() };
    let entry_sq = squash(&entry);
    let info_sq = squash(&info);
    for (file, src, needle) in [
        (
            "cli_entry.rs",
            &entry_sq,
            concat!("println!(", "\"File: {}\""),
        ),
        (
            "cli_entry.rs",
            &entry_sq,
            concat!("println!(", "\"Duration: {}"),
        ),
        (
            "cli_entry.rs",
            &entry_sq,
            concat!("println!(", "\"Streams: {}\""),
        ),
        (
            "info.rs",
            &info_sq,
            concat!("println!(", "\"Submitted — thank"),
        ),
        (
            "info.rs",
            &info_sq,
            concat!("eprintln!(", "\"Cannot write {}"),
        ),
        (
            "info.rs",
            &info_sq,
            concat!("eprint!(", "\"Submit this profile"),
        ),
    ] {
        let needle = squash(needle);
        assert!(
            !src.contains(&needle),
            "{file} prints `{needle}` directly — that is one subcommand with \
                 two languages, which is the drift the shared catalog exists to \
                 stop"
        );
    }

    for (file, src, key) in [
        ("cli_entry.rs", &entry, "\"disc.file\""),
        ("cli_entry.rs", &entry, "\"disc.duration\""),
        ("cli_entry.rs", &entry, "\"disc.streams\""),
        ("info.rs", &info, "\"drive.submit_prompt\""),
        ("info.rs", &info, "\"drive.submit_thanks\""),
        ("info.rs", &info, "\"drive.submit_auto_failed\""),
        ("info.rs", &info, "\"drive.submit_declined\""),
        ("info.rs", &info, "\"error.cannot_write\""),
    ] {
        assert!(
            src.contains(key),
            "{file} no longer looks up {key} — the line it rendered has \
                 either gone silent or gone back to English"
        );
    }
}

/// `freemkv <cmd> --help` routes to the per-command help before the
/// command's own parser runs. Forced `true`, `freemkv info disc://` prints
/// help instead of scanning the disc.
#[test]
fn help_is_requested_only_by_the_help_flags() {
    assert!(!wants_help(&v([].as_slice())));
    assert!(!wants_help(&v(&["disc://"])));
    assert!(!wants_help(&v(&["--helpful", "-help", "h"])));
    assert!(wants_help(&v(&["--help"])));
    assert!(wants_help(&v(&["-h"])));
    assert!(wants_help(&v(&["disc://", "-h"])));
    assert!(wants_help(&v(&["--share", "--help", "-m"])));
}
