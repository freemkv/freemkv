use super::{
    KeyConfig, PipeFail, build_jobs, build_key_sources_quiet, copy_should_continue,
    dest_is_directory, disc_copy_recovered_data, disc_title_nums, fmt_disc_damage, fmt_err,
    fmt_err_str, is_keyserver_url, is_metadata_sink, is_scheme_only_sink, is_url_token,
    parse_error_code, parse_flags, parse_stream_spec, preflight_validate, render_error,
    resolved_keydb_path, sanitize_name, scan_failed_msg, title_in_range, validate_dir_input,
    validate_file_dest, validate_iso_input,
};

#[test]
fn a_dir_source_must_exist_and_be_a_directory() {
    let base = std::env::temp_dir().join(format!("fmkv-dirval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    // A real directory passes.
    assert!(validate_dir_input(&base).is_ok());

    // A path that is not there is refused, and names itself.
    let missing = base.join("no-such-folder");
    let err = validate_dir_input(&missing).expect_err("a missing folder must be refused");
    assert!(
        err.contains(&missing.display().to_string()),
        "the message must name the path the user typed, got: {err}"
    );

    // A FILE where a folder was meant is refused — the mistake a user
    // actually makes is pointing dir:// at the .iso next to the folder.
    let file = base.join("VIDEO_TS.iso");
    std::fs::write(&file, b"not a folder").unwrap();
    assert!(
        validate_dir_input(&file).is_err(),
        "a file is not a folder and must not reach open"
    );

    let _ = std::fs::remove_dir_all(&base);
}
use crate::output::Output;
use crate::strings;
use libfreemkv::parse_url;

// ── The `--help` examples must actually run ─────────────────────────────
// Nothing connected `usage.ex.*` strings to the parser, so one shipped
// rejected by `build_jobs`. Drives each through the real parser here instead.

/// Split a `usage.ex.*` line into its command tokens, dropping the leading
/// `freemkv` and the trailing right-hand description column (separated from
/// the command by a run of two or more spaces).
fn example_argv(line: &str) -> Vec<String> {
    let cmd = line.trim_start().split("  ").next().unwrap_or("").trim();
    cmd.split_whitespace()
        .skip(1) // the `freemkv` program name
        .map(str::to_string)
        .collect()
}

/// Every rip example printed by `usage()`, straight from the English
/// catalogue — the exact text a user reads from `freemkv --help`.
fn shipped_rip_examples() -> Vec<(&'static str, String)> {
    let en: serde_json::Value =
        serde_json::from_str(freemkv_i18n::bundled_locale_json("en").expect("en bundled"))
            .expect("en.json parses");
    // The keys `usage()` prints, in order. `info` is not a rip (no
    // destination URL) and is exercised by the `info` route's own tests.
    let keys = [
        "rip_mkv",
        "rip_m2ts",
        "rip_drive",
        "rip_title",
        "rip_titles",
        "rip_iso",
        "rip_iso_raw",
        "rip_iso_mp",
        "iso_to_mkv",
        "network",
        "network_recv",
        "stdio",
        "benchmark",
    ];
    keys.iter()
        .map(|k| {
            let line = en
                .get("usage")
                .and_then(|u| u.get("ex"))
                .and_then(|e| e.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    panic!(
                        "usage.ex.{k} is listed here but missing from en.json — \
                             keep this list in step with the block `usage()` prints"
                    )
                });
            (*k, line.to_string())
        })
        .collect()
}

#[test]
fn shipped_help_examples_parse_and_build_runnable_jobs() {
    let found = shipped_rip_examples();
    // A scratch root so a directory-style example's `create_dir_all` lands
    // in target/, not the crate root.
    let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(format!("help_examples_{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch dir");

    let out = Output::new(false, true);
    for (key, line) in &found {
        let argv = example_argv(line);
        assert!(!argv.is_empty(), "usage.ex.{key}: no command in {line:?}");

        // 1. The flags must parse.
        let flags = parse_flags(&argv)
            .unwrap_or_else(|e| panic!("usage.ex.{key}: flags rejected: {e}\n  {line}"));

        // 2. Source and destination URLs.
        let urls: Vec<&String> = argv.iter().filter(|a| is_url_token(a)).collect();
        assert_eq!(
            urls.len(),
            2,
            "usage.ex.{key}: expected a source and a destination URL, got {urls:?}"
        );
        let (source, dest) = (urls[0].as_str(), urls[1].as_str());
        let is_disc = matches!(parse_url(source), libfreemkv::StreamUrl::Disc { .. });

        // Relocate a filesystem destination under the scratch root so the
        // example's own path is never created in the crate root. The
        // trailing slash (and hence its directory-ness) is preserved.
        let parsed_shipped = parse_url(dest);
        let dest_owned = match parsed_shipped {
            libfreemkv::StreamUrl::Mkv { .. }
            | libfreemkv::StreamUrl::M2ts { .. }
            | libfreemkv::StreamUrl::Iso { .. } => format!(
                "{}://{}/{}",
                parsed_shipped.scheme(),
                scratch.display(),
                parsed_shipped.path_str()
            ),
            _ => dest.to_string(),
        };
        let parsed_dest = parse_url(&dest_owned);

        // 2b. The invocation must pass the upfront validator, with a source that exists.
        let parsed_src = parse_url(source);
        let source_owned = match &parsed_src {
            libfreemkv::StreamUrl::Iso { .. } => {
                let f = scratch.join(format!("src-{key}.iso"));
                std::fs::write(&f, [0u8; 16]).expect("scratch source");
                format!("iso://{}", f.display())
            }
            // A named device would have to exist; auto-detect has the same rules.
            libfreemkv::StreamUrl::Disc { .. } => "disc://".to_string(),
            _ => source.to_string(),
        };
        let selection_flags_used =
            !flags.streams.is_all() || !flags.title_nums.is_empty() || flags.all_titles;
        if let Err(msg) = preflight_validate(
            &source_owned,
            &dest_owned,
            &parse_url(&source_owned),
            &parsed_dest,
            flags.raw,
            flags.multipass,
            flags.force,
            selection_flags_used,
        ) {
            panic!("usage.ex.{key} is printed by `freemkv --help` but is refused: {msg}\n  {line}");
        }

        // 3. The job set must build. `None` is the CLI's hard rejection —
        //    the exact path `-t 1 -t 3` into `mkv://Movie.mkv` took.
        let titles = None;
        let jobs = build_jobs(
            &titles,
            is_disc,
            &flags.title_nums,
            dest_is_directory(&dest_owned, &parsed_dest),
            &dest_owned,
            &parsed_dest,
            &out,
        );
        assert!(
            jobs.is_some(),
            "usage.ex.{key} is printed by `freemkv --help` but the CLI rejects it:\n  {line}"
        );
        // Every requested title must get its own job — an example that
        // asks for two titles and silently produces one is still wrong.
        if flags.title_nums.len() > 1 {
            assert_eq!(
                jobs.as_ref().unwrap().len(),
                flags.title_nums.len(),
                "usage.ex.{key}: {} titles requested, {} job(s) built:\n  {line}",
                flags.title_nums.len(),
                jobs.as_ref().unwrap().len()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn no_two_help_examples_show_the_same_command() {
    // `rip_iso_mp` and `rip_iso_patch` rendered byte-identical commands under
    // different descriptions, so `--help` showed the same invocation twice and
    // one of the two descriptions was necessarily wrong about it.
    let mut seen: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for (key, line) in shipped_rip_examples() {
        let cmd = example_argv(&line).join(" ");
        if let Some(prev) = seen.insert(cmd.clone(), key) {
            panic!(
                "usage.ex.{prev} and usage.ex.{key} print the SAME command \
                     under different descriptions: `freemkv {cmd}`"
            );
        }
    }
}

// ── `-t` default (1.6.0): main title unless `-t N` / `-t all` ──────────
// (normalization lives in `run()`; this pins the parse layer only)

#[test]
fn t_all_on_a_disc_expands_to_every_title() {
    // The whole disc.
    assert_eq!(disc_title_nums(true, &[], 12), (1..=12).collect::<Vec<_>>());
    assert_eq!(disc_title_nums(true, &[], 1), vec![1]);

    // An explicit selection wins — `-t 2 -t 5 --title all` keeps the two.
    assert_eq!(disc_title_nums(true, &[2, 5], 12), vec![2, 5]);
    // Without `-t all` nothing is expanded, whatever the count.
    assert_eq!(disc_title_nums(false, &[3], 12), vec![3]);
    assert_eq!(disc_title_nums(false, &[], 12), Vec::<usize>::new());
    // A scan that found nothing expands to nothing rather than [1..=0].
    assert_eq!(disc_title_nums(true, &[], 0), Vec::<usize>::new());
}

/// The expansion must route into the multi-title DISC arm of `build_jobs`,
/// one job per title — not the single-job catch-all.
#[test]
fn an_expanded_disc_selection_builds_one_job_per_title() {
    let out = Output::new(false, true);
    let dir = temp_path("t-all-jobs");
    let dest = &format!("{}/", dir.display());
    let parsed = libfreemkv::parse_url(dest);
    let nums = disc_title_nums(true, &[], 4);
    let jobs = build_jobs(&None, true, &nums, true, dest, &parsed, &out)
        .expect("a directory dest accepts a multi-title disc rip");
    assert_eq!(jobs.len(), 4, "expected one job per title, got {jobs:?}");
    // 1-based flags map onto 0-based indices, in order.
    let idx: Vec<Option<usize>> = jobs.iter().map(|(i, _)| *i).collect();
    assert_eq!(idx, vec![Some(0), Some(1), Some(2), Some(3)]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A single-title disc collapses to one job — same as `-t 1`, not a
/// directory of one.
#[test]
fn t_all_on_a_one_title_disc_is_a_single_job() {
    let out = Output::new(false, true);
    let dest = &temp_path("t-all-one.mkv").display().to_string();
    let parsed = libfreemkv::parse_url(dest);
    let nums = disc_title_nums(true, &[], 1);
    let jobs = build_jobs(&None, true, &nums, false, dest, &parsed, &out)
        .expect("a single title may go to a single file");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].0, Some(0));
}

#[test]
fn the_t_default_normalizes_to_the_main_title_only() {
    // The rule `run()` itself applies, called directly. It used to be
    // re-stated as a local copy here, which proved nothing about `run()` —
    // both mutants of the real line survived this test.
    fn normalize(mut nums: Vec<usize>, all_titles: bool) -> Vec<usize> {
        super::normalize_title_nums(&mut nums, all_titles);
        nums
    }
    // No flags at all -> main title only, NOT every title.
    assert_eq!(normalize(vec![], false), vec![1]);
    // `-t all` must NOT be normalized to [1]; empty means all-titles
    // downstream, and injecting [1] here would silently rip one title.
    assert_eq!(normalize(vec![], true), Vec::<usize>::new());
    // An explicit selection is left exactly as given.
    assert_eq!(normalize(vec![3], false), vec![3]);
    assert_eq!(normalize(vec![2, 5], false), vec![2, 5]);
    // `-t all` alongside explicit numbers keeps the numbers.
    assert_eq!(normalize(vec![2], true), vec![2]);
}

#[test]
fn t_all_sets_all_titles_flag_and_no_nums() {
    let f = parse_flags(&["-t".into(), "all".into()]).unwrap();
    assert!(f.all_titles, "-t all must set all_titles");
    assert!(
        f.title_nums.is_empty(),
        "-t all carries no explicit numbers"
    );
}

#[test]
fn t_all_is_case_insensitive() {
    assert!(
        parse_flags(&["-t".into(), "ALL".into()])
            .unwrap()
            .all_titles
    );
    assert!(
        parse_flags(&["--title".into(), "All".into()])
            .unwrap()
            .all_titles
    );
}

#[test]
fn no_title_flag_leaves_empty_nums_and_not_all() {
    // run() then normalizes this to [1] (main title). parse_flags itself
    // leaves it empty + all_titles=false — the state the default keys off.
    let f = parse_flags(&["--raw".into()]).unwrap();
    assert!(f.title_nums.is_empty());
    assert!(!f.all_titles);
}

#[test]
fn explicit_t_number_still_parses() {
    let f = parse_flags(&["-t".into(), "3".into()]).unwrap();
    assert_eq!(f.title_nums, vec![3]);
    assert!(!f.all_titles);
}

#[test]
fn t_zero_still_rejected() {
    // `-t 0` remains invalid (1-based); `all` is the way to get everything.
    assert!(parse_flags(&["-t".into(), "0".into()]).is_err());
}

// ── `-a`/`-s` stream selection ─────────────────────────────────────────────
use freemkv_engine::{StreamFilter, SubtitleFilter};

#[test]
fn absent_a_s_flags_default_to_all() {
    let f = parse_flags(&["--raw".into()]).unwrap();
    assert_eq!(f.streams.audio, StreamFilter::All);
    assert_eq!(f.streams.subtitles, StreamFilter::All.into());
    assert!(f.streams.is_all());
}

#[test]
fn audio_langs_parse_into_a_lang_list() {
    let f = parse_flags(&["-a".into(), "eng,spa".into()]).unwrap();
    assert_eq!(
        f.streams.audio,
        StreamFilter::Langs(vec!["eng".into(), "spa".into()])
    );
    // subtitles untouched.
    assert_eq!(f.streams.subtitles, StreamFilter::All.into());
}

#[test]
fn subtitle_flag_sets_only_subtitles() {
    let f = parse_flags(&["-s".into(), "English".into()]).unwrap();
    assert_eq!(
        f.streams.subtitles,
        SubtitleFilter::from(StreamFilter::Langs(vec!["English".into()]))
    );
    assert_eq!(f.streams.audio, StreamFilter::All);
}

#[test]
fn spec_keywords_all_and_none_are_case_insensitive() {
    assert_eq!(parse_stream_spec("all"), StreamFilter::All);
    assert_eq!(parse_stream_spec("ALL"), StreamFilter::All);
    assert_eq!(parse_stream_spec("none"), StreamFilter::None);
    assert_eq!(parse_stream_spec("None"), StreamFilter::None);
}

#[test]
fn spec_trims_and_drops_empty_langs() {
    assert_eq!(
        parse_stream_spec(" eng , , spa "),
        StreamFilter::Langs(vec!["eng".into(), "spa".into()])
    );
}

#[test]
fn a_flag_value_is_not_swallowed_as_a_url() {
    // `-a eng` between two URLs: the value must be consumed, not left to be
    // mistaken for a positional stream URL.
    let f = parse_flags(&["-a".into(), "eng".into(), "iso://x".into()]).unwrap();
    assert_eq!(f.streams.audio, StreamFilter::Langs(vec!["eng".into()]));
}

#[test]
fn a_flag_needs_a_value() {
    // `-a` with a URL immediately after (no value) is an error.
    assert!(parse_flags(&["-a".into(), "iso://x".into(), "mkv://o".into()]).is_err());
}

// The decrypt no-key verdict matrix now lives in `libfreemkv::Disc::
// ensure_decryptable[_keys]`; no-raw-code-leak is covered below.

#[test]
fn pipefail_classifies_via_the_engine() {
    use freemkv_engine::TitleResult;
    let r = |e: libfreemkv::Error| PipeFail::from_mux(e.into()).result;
    // Skippable stubs (E7023 CssKeyMissing, E6008 MkvInvalid).
    assert_eq!(
        r(libfreemkv::Error::CssKeyMissing),
        TitleResult::SkippableStub
    );
    assert_eq!(r(libfreemkv::Error::MkvInvalid), TitleResult::SkippableStub);
    // Disc-level no-key (E7022 NoDiscKey, E7000 AacsNoKeys) → fail-fast.
    assert_eq!(
        r(libfreemkv::Error::NoDiscKey {
            disc_hash: "abcd1234".into()
        }),
        TitleResult::DiscLevelNoKey
    );
    assert_eq!(
        r(libfreemkv::Error::AacsNoKeys),
        TitleResult::DiscLevelNoKey
    );
    // A real non-stub failure is Failed (never silently skipped).
    assert_eq!(r(libfreemkv::Error::NoStreams), TitleResult::Failed);
    assert_eq!(
        PipeFail::from_mux(std::io::Error::other("boom")).result,
        TitleResult::Failed
    );
    // Halt (Ctrl-C) and typed-error constructors classify correctly too.
    assert_eq!(
        PipeFail::halted("interrupted".into()).result,
        TitleResult::Halted
    );
    assert_eq!(
        PipeFail::from_typed(libfreemkv::Error::NoDiscKey {
            disc_hash: "x".into()
        })
        .result,
        TitleResult::DiscLevelNoKey
    );
    // A plain fatal setup failure is Failed.
    assert_eq!(PipeFail::fatal("boom".into()).result, TitleResult::Failed);
}

// A halted tree extraction is not resumable: no "Progress kept" even though the dir exists.
#[test]
fn a_halted_tree_does_not_claim_kept_progress() {
    let dir = temp_path("halted-tree");
    std::fs::create_dir_all(&dir).unwrap();
    let tree = freemkv_engine::Output::Tree { path: dir.clone() };
    assert_eq!(
        super::halted_text(&tree, &dir),
        crate::strings::get("rip.interrupted")
    );
    let image = freemkv_engine::Output::Image {
        path: dir.clone(),
        null: false,
    };
    assert_ne!(
        super::halted_text(&image, &dir),
        crate::strings::get("rip.interrupted")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Under -q a failure is still reported (the exit code is 1) and a lossy tree still names
// its holes, with the disc-derived path neutralised.
#[test]
fn errors_and_loss_show_under_quiet() {
    let quiet = Output::new(false, true);
    let ((), printed) = crate::output::capture(|| {
        let r = super::report_keys(Err(libfreemkv::Error::NoStreams), &quiet);
        assert!(r.is_none());
    });
    assert!(!printed.is_empty(), "a key refusal must show under -q");
    let res = libfreemkv::ExtractResult {
        files: vec![libfreemkv::FileResult {
            path: "a\u{1b}[2Jb".into(),
            bytes_good: 0,
            bytes_unreadable: 4096,
            complete: false,
        }],
        ..Default::default()
    };
    let (ok, printed) = crate::output::capture(|| super::render_extract(&res, &quiet));
    assert!(!ok);
    assert!(!printed.contains('\u{1b}'), "{printed:?}");
    assert!(printed.lines().count() >= 2, "{printed:?}");
}

// A Ctrl-C while a title reopens the drive is a full stop, not a failed title.
#[test]
fn a_ctrl_c_during_the_drive_reopen_is_a_halt() {
    use freemkv_engine::TitleResult;
    assert_eq!(
        super::reopen_failure(&libfreemkv::Error::Halted).result,
        TitleResult::Halted
    );
    assert_eq!(
        super::reopen_failure(&libfreemkv::Error::DeviceNotFound {
            path: String::new()
        })
        .result,
        TitleResult::Failed
    );
}

// A panic elsewhere must not turn every later progress event into another panic.
#[test]
fn a_poisoned_progress_mutex_does_not_panic_the_event_callbacks() {
    let events = std::sync::Arc::new(super::CliMuxEvents::new(
        Output::new(false, false),
        "mkv://x".into(),
        false,
    ));
    let e = events.clone();
    let _ = std::thread::spawn(move || {
        let _held = e.start.lock().unwrap();
        let _also = e.last_update.lock().unwrap();
        panic!("poison both");
    })
    .join();
    assert!(events.start().is_none());
    events.on_write_progress(1, 2);
}

// The skip/stop/fail POLICY (decide_title) is unit-tested in freemkv-engine;
// the CLI only classifies PipeFail into a TitleResult (above), so it doesn't re-test the policy.

#[test]
fn metadata_sink_detected_for_chapters_and_json() {
    assert!(is_metadata_sink("chapters:///tmp/out.xml"));
    assert!(is_metadata_sink("json:///tmp/out.json"));
    assert!(!is_metadata_sink("mkv:///tmp/out.mkv"));
    assert!(!is_metadata_sink("null://"));
}

/// The skip warning the loop prints (`rip.title_skipped`) exists in en.json
/// and carries the `{num}` placeholder — so a skipped title surfaces a clear,
/// localized, non-error message rather than a raw E7023.
#[test]
fn rip_title_skipped_string_present_and_localized() {
    let s = strings::fmt("rip.title_skipped", &[("num", "3")]);
    assert_ne!(s, "rip.title_skipped", "missing locale entry");
    assert!(s.contains('3'), "num placeholder not substituted: {s}");
    assert!(
        !s.contains("E7023") && !s.to_lowercase().contains("error"),
        "skip notice must not look like a hard error: {s}"
    );
}

#[test]
fn disc_copy_recovered_data_gates_zero_recovery() {
    // Whole disc unreadable → no data recovered → not a success.
    assert!(!disc_copy_recovered_data(0));
    // Any recovered bytes → success.
    assert!(disc_copy_recovered_data(1));
    assert!(disc_copy_recovered_data(50_000_000_000));
}

// The header-resolution gate that used to live in the CLI now lives in
// `libfreemkv::mux::mux_with_keys`, covered by its own tests there.

// ── fmt_err generalization (english errors for ALL codes) ───────────────

/// `parse_error_code` splits the libfreemkv `E<code>[: <data>]` Display
/// form into the code token and its trailing data.
#[test]
fn parse_error_code_splits_code_and_data() {
    assert_eq!(parse_error_code("E6009"), Some(("E6009", "")));
    assert_eq!(parse_error_code("E7022: abcdef"), Some(("E7022", "abcdef")));
    assert_eq!(parse_error_code("E5000: 13"), Some(("E5000", "13")));
    // Not an E-code: returns None (falls through to the generic wrapper).
    assert_eq!(parse_error_code("No drive found"), None);
    assert_eq!(parse_error_code("Error: boom"), None);
    assert_eq!(parse_error_code("E"), None);
    assert_eq!(parse_error_code("Eabc"), None);
}

#[test]
fn fmt_err_renders_codes_to_english() {
    // E6009 NoStreams — the Theme A zero-output error. Code now prefixed,
    // message dejargoned to the user-facing "no audio or video streams".
    let s = fmt_err_str("E6009");
    assert!(s.starts_with("E6009 "), "code not prefixed: {s}");
    assert!(
        s.to_lowercase().contains("no audio or video streams"),
        "got: {s}"
    );

    // E7023 CssKeyMissing — the Theme B CSS gate error. The user-facing
    // copy is dejargoned: "copy-protected", not "CSS title key".
    let s = fmt_err_str("E7023");
    assert!(s.starts_with("E7023 "), "code not prefixed: {s}");
    assert!(s.to_lowercase().contains("copy-protected"), "got: {s}");

    // E9023 MuxEmpty — the Theme A m2ts zero-frame error. Dejargoned to
    // "empty file" / "video or audio", not the internal "mux" term.
    let s = fmt_err_str("E9023");
    assert!(s.starts_with("E9023 "), "code not prefixed: {s}");
    assert!(s.to_lowercase().contains("empty file"), "got: {s}");

    // E5000 with data → {detail} substituted, code prefixed.
    let s = fmt_err_str("E5000: 13");
    assert!(s.starts_with("E5000 "), "code not prefixed: {s}");
    assert!(s.contains("13"), "detail not substituted: {s}");

    // E7013 Decryption failed — code now prefixed.
    let s = fmt_err_str("E7013");
    assert!(s.starts_with("E7013 "), "code not prefixed: {s}");
    assert!(s.to_lowercase().contains("decryption failed"), "got: {s}");

    // E7022 names the disc by hash, code prefixed.
    let s = fmt_err_str("E7022: deadbeef");
    assert!(s.starts_with("E7022 "), "code not prefixed: {s}");
    assert!(s.contains("deadbeef"), "hash not substituted: {s}");
}

#[test]
fn key_service_failures_do_not_render_as_a_missing_disc_key() {
    let missing = fmt_err_str("E7022: 422eb0");
    let unavailable = fmt_err_str("E7028");
    let unauthorized = fmt_err_str("E7029");
    let rate_limited = fmt_err_str("E7030");

    for (code, s) in [
        ("E7028", &unavailable),
        ("E7029", &unauthorized),
        ("E7030", &rate_limited),
    ] {
        assert!(s.starts_with(&format!("{code} ")), "code not prefixed: {s}");
        // Not the generic wrapper — a real locale entry exists.
        assert!(
            !s.contains(&format!("error.{code}")),
            "{code} fell through to the raw key path: {s}"
        );
        assert_ne!(*s, missing, "{code} must not reuse E7022's message");
        // Never the sentence that sent the operator hunting for a VUK.
        assert!(
            !s.to_lowercase()
                .contains("no key source has a decryption key"),
            "{code} must not claim the disc has no key: {s}"
        );
    }

    // Each names its own action, and the three are distinct from each other.
    assert!(
        unavailable.to_lowercase().contains("try again"),
        "E7028 must tell the operator to retry: {unavailable}"
    );
    assert!(
        unauthorized.to_lowercase().contains("token"),
        "E7029 must point at the credentials: {unauthorized}"
    );
    assert!(
        rate_limited.to_lowercase().contains("rate-limiting"),
        "E7030 must name the rate limit: {rate_limited}"
    );
    assert_ne!(unavailable, unauthorized);
    assert_ne!(unauthorized, rate_limited);
}

/// The full render-site output: `render_error` prefixes the `Error:` level
/// word exactly once onto the `E<code> <message>` fragment (WS2 §2.1).
#[test]
fn render_error_prefixes_level_once() {
    let rendered = render_error(&"E6009");
    assert!(rendered.starts_with("Error: E6009 "), "got: {rendered}");
    // The level word appears exactly once (no nested doubling).
    assert_eq!(rendered.matches("Error:").count(), 1);
}

/// L4c: a scan failure's cause is localized, never the raw E-code Display.
#[test]
fn scan_failed_localizes_its_cause() {
    let s = scan_failed_msg(&"E6000: 7476928 0x02/0x03/0x11/0x00");
    assert!(!s.contains("0x"), "raw sense tail leaked: {s}");
    assert!(
        s.contains(&fmt_err_str("E6000: 7476928 0x02/0x03/0x11/0x00")),
        "{s}"
    );
}

/// E6000 (DiscRead) Display is `E6000: <sector> 0x..status../0x..sense..`.
/// The status/sense hex tail is diagnostic noise that must NOT reach the
/// user — only the sector number is substituted into the localized message.
#[test]
fn fmt_err_e6000_strips_status_sense_hex_tail() {
    // Full DiscRead Display: sector + status + sense triple. The code is
    // now shown as a prefix; the status/sense hex tail is still stripped.
    let s = fmt_err_str("E6000: 7476928 0x02/0x03/0x11/0x00");
    assert!(s.starts_with("E6000 "), "code not prefixed: {s}");
    assert!(s.contains("7476928"), "sector number lost: {s}");
    assert!(!s.contains("0x"), "raw hex tail leaked to user: {s}");
    // Sense-only form (no status byte) also strips the tail.
    let s = fmt_err_str("E6000: 100 0x03/0x11/0x00");
    assert!(s.contains("100") && !s.contains("0x"), "got: {s}");
    // Bare sector (no tail at all) renders cleanly.
    let s = fmt_err_str("E6000: 42");
    assert!(s.contains("42") && !s.contains("0x"), "got: {s}");
}

#[test]
fn fmt_err_unknown_code_uses_generic_wrapper() {
    // E1234 has no locale entry; the generic wrapper keeps the code.
    let s = fmt_err_str("E1234: whatever");
    assert_eq!(s, "E1234 whatever");
    // Through the render site the code is still shown with the level word.
    assert_eq!(render_error(&"E1234: whatever"), "Error: E1234 whatever");
}

/// A non-code error string (e.g. a CLI-side message) passes through the
/// generic wrapper with an empty code, so `fmt_err_str` yields the bare
/// string and the render site prefixes the level word.
#[test]
fn fmt_err_non_code_string_uses_generic() {
    // Empty code → leading space trimmed away by the render contract; the
    // fragment carries just the message.
    let s = fmt_err_str("No BD drive found");
    assert!(s.contains("No BD drive found"), "got: {s}");
    assert!(!s.contains('E'), "no spurious code token: {s}");
    assert_eq!(
        render_error(&"No BD drive found"),
        "Error: No BD drive found"
    );
}

// ── negative path: no-keydb AACS disc → E7022 surfaced in English ───────

#[test]
fn no_keydb_aacs_disc_surfaces_e7022_in_english() {
    // The error pipe_disc returns, rendered for the user.
    let disp = libfreemkv::Error::NoDiscKey {
        disc_hash: "deadbeefcafe".to_string(),
    }
    .to_string();
    assert!(
        disp.starts_with("E7022"),
        "library Display is E7022: {disp}"
    );
    let rendered = fmt_err_str(&disp);
    // English, names the disc by hash, code SHOWN (WS2: code-forward).
    assert!(rendered.contains("deadbeefcafe"), "hash named: {rendered}");
    assert!(
        rendered.starts_with("E7022 "),
        "code not prefixed: {rendered}"
    );
    assert!(
        rendered.to_lowercase().contains("key"),
        "english key message: {rendered}"
    );
}

#[test]
fn copy_halts_on_first_interrupt() {
    // The Ctrl-C fix: the copy progress callback must return false (halt) the
    // moment SIGINT is seen, so the first Ctrl-C stops the sweep and the
    // tray unlocks on drop — rather than being ignored until `_exit(130)`.
    assert!(copy_should_continue(false), "no interrupt → keep going");
    assert!(!copy_should_continue(true), "interrupt → halt the copy");
}

// The `mux_was_interrupted` check that used to live here is gone: the CLI
// no longer runs the frame loop. `mux_with_keys` polls `libfreemkv::Halt` and
// reports interrupts via `MuxOutcome`, mapped to `interrupted_error`.

#[test]
fn work_pct_is_finite_when_work_total_zero() {
    // `print_disc_progress` now derives `pct` from `PassProgress::work_pct()`,
    // which guards `work_total == 0` (returns 100.0). The old inline
    // `work_done / work_total` produced `NaN%` for an empty Sweep/Mux pass.
    let p = libfreemkv::progress::PassProgress {
        kind: libfreemkv::progress::PassKind::Sweep,
        work_done: 0,
        work_total: 0,
        bytes_good_total: 0,
        bytes_unreadable_total: 0,
        bytes_pending_total: 0,
        bytes_retryable_total: 0,
        bytes_total_disc: 0,
        disc_duration_secs: None,
        bytes_bad_in_main_title: 0,
        main_title_duration_secs: None,
        main_title_size_bytes: None,
        located: Default::default(),
    };
    let pct = p.work_pct();
    assert!(pct.is_finite(), "work_total==0 must not yield NaN%");
    assert_eq!(pct, 100.0);
    // And the CLI line built from it, for a pass with a known disc size.
    let mut p = p;
    p.bytes_total_disc = 10_000;
    let line = super::disc_progress_line(&p, 0, None).expect("the disc size is known");
    assert!(!line.contains("NaN"), "{line}");
    assert!(line.contains("(100.0%)"), "{line}");
}

#[test]
fn disc_damage_unread_is_not_lost() {
    let p = libfreemkv::progress::PassProgress {
        kind: libfreemkv::progress::PassKind::Sweep,
        work_done: 800,
        work_total: 10_000,
        bytes_good_total: 800,
        bytes_unreadable_total: 0,
        // 92% of the disc not yet read — large pending, but ZERO failed.
        bytes_pending_total: 9_200,
        bytes_retryable_total: 0,
        bytes_total_disc: 10_000,
        disc_duration_secs: Some(7200.0),
        bytes_bad_in_main_title: 0,
        main_title_duration_secs: Some(7200.0),
        main_title_size_bytes: Some(10_000),
        located: Default::default(),
    };
    let damage = fmt_disc_damage(&p);
    assert_eq!(
        damage,
        strings::get("rip.damage_none"),
        "unread sectors must not count as lost; got {damage:?}"
    );
    assert!(
        !damage.contains("lost"),
        "healthy rip must not render a 'lost' string; got {damage:?}"
    );
}

/// Sectors that actually FAILED to read (unreadable, or retryable =
/// NonTrimmed/NonScraped awaiting retry) DO count as lost.
#[test]
fn disc_damage_failed_reads_are_lost() {
    // Retryable (failed-awaiting-retry) alone triggers "lost".
    let p_retryable = libfreemkv::progress::PassProgress {
        kind: libfreemkv::progress::PassKind::Sweep,
        work_done: 5_000,
        work_total: 10_000,
        bytes_good_total: 4_900,
        bytes_unreadable_total: 0,
        bytes_pending_total: 5_100,
        bytes_retryable_total: 100,
        bytes_total_disc: 10_000,
        disc_duration_secs: Some(7200.0),
        bytes_bad_in_main_title: 0,
        main_title_duration_secs: Some(7200.0),
        main_title_size_bytes: Some(10_000),
        located: Default::default(),
    };
    let damage = fmt_disc_damage(&p_retryable);
    assert!(
        damage.contains("lost"),
        "failed-awaiting-retry must render a 'lost' string; got {damage:?}"
    );
    assert_ne!(damage, strings::get("rip.damage_none"));

    // Unreadable (gave up) alone also triggers "lost".
    let p_unreadable = libfreemkv::progress::PassProgress {
        bytes_unreadable_total: 100,
        bytes_retryable_total: 0,
        ..p_retryable
    };
    assert!(
        fmt_disc_damage(&p_unreadable).contains("lost"),
        "unreadable bytes must render a 'lost' string"
    );
}

fn v(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

#[test]
fn stream_info_uses_dedicated_keys() {
    // Regression: `print_stream_info` mislabeled the track count with
    // `disc.titles` and the runtime with `disc.format`. Both now have
    // dedicated keys, which must not equal their own dotted path (a miss).
    assert_ne!(crate::strings::get("disc.streams"), "disc.streams");
    assert_ne!(crate::strings::get("disc.duration"), "disc.duration");
    // And they must be distinct from the keys they were confused with, so a
    // future copy-paste can't silently re-alias them.
    assert_ne!(
        crate::strings::get("disc.streams"),
        crate::strings::get("disc.titles")
    );
    assert_ne!(
        crate::strings::get("disc.duration"),
        crate::strings::get("disc.format")
    );
}

#[test]
fn url_token_detection() {
    assert!(is_url_token("disc://"));
    assert!(is_url_token("mkv://out.mkv"));
    assert!(!is_url_token("1"));
    assert!(!is_url_token("keydb.cfg"));
    assert!(!is_url_token("/path/out.mkv"));
}

#[test]
fn title_one_based_value_accepted() {
    let f = parse_flags(&v(&["-t", "1", "-t", "3"])).unwrap();
    assert_eq!(f.title_nums, vec![1, 3]);
}

#[test]
fn duplicate_title_flags_dedup() {
    // `-t 1 -t 1` must collapse to a single title, not two jobs that both
    // map to the same index and overwrite the same output file.
    let f = parse_flags(&v(&["-t", "1", "-t", "1"])).unwrap();
    assert_eq!(f.title_nums, vec![1]);
    // Out-of-order repeats sort + dedup deterministically.
    let f = parse_flags(&v(&["-t", "3", "-t", "1", "-t", "3"])).unwrap();
    assert_eq!(f.title_nums, vec![1, 3]);
}

#[test]
fn disc_multiple_titles_build_one_job_each() {
    // Regression (HIGH): multiple `-t` on a disc source must build one
    // job per requested title, not silently drop all but the first.
    let out = Output::new(false, true);
    // Repo-local scratch (not /tmp): survives reboots and stays inside the
    // build tree so stray dirs are obvious and cleaned by `cargo clean`.
    let dest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(format!("freemkv_test_{}", std::process::id()));
    let dest = format!("mkv://{}", dest_dir.display());
    let parsed_dest = libfreemkv::parse_url(&dest);

    let jobs = build_jobs(
        &None,
        true, // is_disc
        &[1usize, 3usize],
        true, // is_dir_dest — multiple titles require a directory dest
        &dest,
        &parsed_dest,
        &out,
    )
    .expect("dir creation should succeed in temp");

    assert_eq!(jobs.len(), 2, "both -t 1 and -t 3 must produce a job");
    // Title indices are 0-based: -t 1 → 0, -t 3 → 2.
    assert_eq!(jobs[0].0, Some(0));
    assert_eq!(jobs[1].0, Some(2));
    // Distinct output files (no silent overwrite / drop).
    assert_ne!(jobs[0].1, jobs[1].1);
    assert!(jobs[0].1.contains("_t1."), "got {}", jobs[0].1);
    assert!(jobs[1].1.contains("_t3."), "got {}", jobs[1].1);

    let _ = std::fs::remove_dir_all(&dest_dir);
}

// Several titles of an unscanned disc onto a single-file name: one file per title beside
// it, as for an image or folder (`-t 1 -t 2 mkv://d/movie.mkv` -> `d/movie_t1.mkv`, `_t2`).
#[test]
fn disc_multiple_titles_to_file_dest_fan_out_beside_it() {
    let out = Output::new(false, true);
    let file = temp_path("multi-to-file").join("movie.mkv");
    let _ = std::fs::remove_dir_all(&file);
    let dest = format!("mkv://{}", file.display());
    let parsed_dest = libfreemkv::parse_url(&dest);
    let jobs = build_jobs(&None, true, &[1, 2], false, &dest, &parsed_dest, &out)
        .expect("several disc titles onto a file name fan out, not refuse");
    let made_dir = file.is_dir();
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
    assert!(!made_dir, "the file name is not turned into a directory");
    let beside = |n: u32| {
        format!(
            "mkv://{}",
            file.with_file_name(format!("movie_t{n}.mkv")).display()
        )
    };
    assert_eq!(
        jobs,
        vec![(Some(0), beside(1)), (Some(1), beside(2))],
        "one file per title beside the given name"
    );
}

#[test]
fn out_of_range_title_is_failure() {
    // Regression (HIGH): an explicit `-t` past the last title must be a hard
    // failure (caller sets ok=false → non-zero exit), not a warning that
    // still exits 0. title_in_range gates that branch.
    assert!(title_in_range(0, 3), "first title is in range");
    assert!(title_in_range(2, 3), "last title is in range");
    assert!(!title_in_range(3, 3), "one past the end is out of range");
    assert!(!title_in_range(99, 3), "far past the end is out of range");
    assert!(!title_in_range(0, 0), "no titles → any index out of range");
    // `pipe_disc` applies the SAME rule through this function now; it used
    // to spell the comparison out inline, so hardening one copy left the
    // other open, risking an index panic mid-rip if inverted.
}

#[test]
fn disc_single_title_is_single_file_job() {
    // A single `-t` on a disc keeps the one-file path (no directory).
    let out = Output::new(false, true);
    let parsed_dest = libfreemkv::parse_url("mkv://out.mkv");
    let jobs = build_jobs(
        &None,
        true,
        &[2usize],
        false,
        "mkv://out.mkv",
        &parsed_dest,
        &out,
    )
    .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].0, Some(1));
    assert_eq!(jobs[0].1, "mkv://out.mkv");
}

#[test]
fn title_zero_rejected() {
    // `-t 0` must not underflow to all-titles; it's an explicit error.
    let err = parse_flags(&v(&["-t", "0"])).unwrap_err();
    assert!(err.contains('0'), "got: {err}");
}

#[test]
fn title_non_numeric_rejected() {
    // A bad value must NOT silently leave title_nums empty (= all titles).
    let err = parse_flags(&v(&["-t", "main"])).unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn title_missing_value_rejected() {
    assert!(parse_flags(&v(&["-t"])).is_err());
    // Followed by a URL → value is missing, not the URL.
    assert!(parse_flags(&v(&["-t", "disc://"])).is_err());
}

#[test]
fn keydb_missing_value_rejected() {
    // `--keydb` with no value must not silently fall back to the default keydb.
    assert!(parse_flags(&v(&["--keydb"])).is_err());
    assert!(parse_flags(&v(&["--keydb", "disc://"])).is_err());
}

#[test]
fn keydb_value_accepted() {
    let f = parse_flags(&v(&["--keydb", "/etc/keydb.cfg"])).unwrap();
    assert_eq!(f.keydb_path.as_deref(), Some("/etc/keydb.cfg"));
}

// ── Online key-source flags ────────────────────────────────────────────

#[test]
fn is_keyserver_url_accepts_http_only() {
    assert!(is_keyserver_url("http://keys.example/keys"));
    assert!(is_keyserver_url("https://keys.example/keys"));
    // A stream URL with a non-http scheme is NOT a key-service URL value.
    assert!(!is_keyserver_url("disc://"));
    assert!(!is_keyserver_url("mkv://out.mkv"));
    assert!(!is_keyserver_url("ftp://x/keys"));
    assert!(!is_keyserver_url("--quiet"));
}

#[test]
fn key_url_and_auth_parse() {
    let f = parse_flags(&v(&[
        "--key-url",
        "https://keys.example/keys",
        "--key-auth",
        "tok123",
    ]))
    .unwrap();
    assert_eq!(f.key_url.as_deref(), Some("https://keys.example/keys"));
    assert_eq!(f.key_auth.as_deref(), Some("tok123"));
}

#[test]
fn key_url_missing_or_non_http_value_rejected() {
    // No value at all.
    assert!(parse_flags(&v(&["--key-url"])).is_err());
    // A following stream URL with a non-http scheme is NOT the value —
    // value is missing (must not eat the positional `disc://`).
    assert!(parse_flags(&v(&["--key-url", "disc://"])).is_err());
    // A following flag means the value is missing.
    assert!(parse_flags(&v(&["--key-url", "--quiet"])).is_err());
}

// ── VAL-2 regression: --key-url scheme validation ──────────────────────
// Bug: the guard was `!is_url_token(u)`, making the bad-scheme branch
// `A && !A` — dead code. Fix: guard on `is_keyserver_url(u)` instead.

/// VAL-2: `--key-url ftp://x` — a non-http(s) scheme — must produce the
/// bad-scheme error, NOT "requires a value" (the value was present).
#[test]
fn val2_key_url_ftp_scheme_gives_bad_scheme_error() {
    let err = parse_flags(&v(&["--key-url", "ftp://x"])).unwrap_err();
    // Must contain the bad-scheme message substring, not the generic
    // "requires a value" substring.
    assert!(
        err.contains("http://") || err.contains("https://"),
        "expected bad-scheme error (mentioning http(s)://), got: {err}"
    );
    assert!(
        !err.contains("requires a value"),
        "must NOT produce flag_needs_value when a value was present: {err}"
    );
    // The bad URL itself must appear in the message so the user can see
    // what was rejected.
    assert!(
        err.contains("ftp://x"),
        "rejected URL missing from error: {err}"
    );
}

/// VAL-2: `--key-url disc://` — a stream scheme used as a key-url — must
/// also produce the bad-scheme error. `disc://` contains `://` but is not
/// http(s), so it goes through the bad-scheme arm, not the missing-value arm.
#[test]
fn val2_key_url_disc_scheme_gives_bad_scheme_error() {
    let err = parse_flags(&v(&["--key-url", "disc://"])).unwrap_err();
    assert!(
        err.contains("http://") || err.contains("https://"),
        "expected bad-scheme error (mentioning http(s)://), got: {err}"
    );
    assert!(
        !err.contains("requires a value"),
        "must NOT produce flag_needs_value when a value (with wrong scheme) was present: {err}"
    );
    assert!(
        err.contains("disc://"),
        "rejected URL missing from error: {err}"
    );
}

/// VAL-2 (positive path): `--key-url https://keys.example/keys` must be
/// accepted and stored verbatim.
#[test]
fn val2_key_url_https_accepted() {
    let f = parse_flags(&v(&["--key-url", "https://keys.example/keys"])).unwrap();
    assert_eq!(
        f.key_url.as_deref(),
        Some("https://keys.example/keys"),
        "https key-url must be accepted and stored verbatim"
    );
}

/// VAL-2 (positive path): `--key-url http://keys.example/keys` (plain http)
/// must also be accepted.
#[test]
fn val2_key_url_http_accepted() {
    let f = parse_flags(&v(&["--key-url", "http://keys.example/keys"])).unwrap();
    assert_eq!(
        f.key_url.as_deref(),
        Some("http://keys.example/keys"),
        "http key-url must be accepted and stored verbatim"
    );
}

/// VAL-2 (missing value): bare `--key-url` with no following token must
/// produce the flag_needs_value error (not the bad-scheme error).
#[test]
fn val2_key_url_no_value_gives_needs_value_error() {
    let err = parse_flags(&v(&["--key-url"])).unwrap_err();
    assert!(
        err.contains("requires a value"),
        "bare --key-url must produce flag_needs_value, got: {err}"
    );
}

/// VAL-2 (missing value via flag): `--key-url --quiet` — the value is a
/// flag, not a URL, so it is missing. Must produce flag_needs_value.
#[test]
fn val2_key_url_followed_by_flag_gives_needs_value_error() {
    let err = parse_flags(&v(&["--key-url", "--quiet"])).unwrap_err();
    assert!(
        err.contains("requires a value"),
        "--key-url followed by a flag must produce flag_needs_value, got: {err}"
    );
}

#[test]
fn key_auth_missing_value_rejected() {
    assert!(parse_flags(&v(&["--key-auth"])).is_err());
    // A following stream URL means the token was omitted.
    assert!(parse_flags(&v(&["--key-auth", "disc://"])).is_err());
}

/// T13: `--key-auth --raw` must not swallow `--raw` as the bearer token.
#[test]
fn key_auth_followed_by_flag_gives_needs_value_error() {
    let err = parse_flags(&v(&["--key-auth", "--raw"])).unwrap_err();
    assert!(err.contains("requires a value"), "got: {err}");
}

/// Source assembly per the agreed design — local-first ordering, pinned via
/// each source's stable `label()` (`"keydb"` before `"online"`).
#[test]
fn build_key_sources_orders_local_first() {
    // keydb only → [Keydb]. (Default location is fine; we only inspect order.)
    let s = build_key_sources_quiet(&KeyConfig {
        keydb_path: Some("keydb.cfg".into()),
        key_url: None,
        key_auth: None,
    });
    assert_eq!(s.len(), 1);
    assert_eq!(
        s[0].label(),
        "keydb",
        "keydb-only first source is the keydb"
    );

    // neither flag → still [Keydb] (default keydb location).
    let s = build_key_sources_quiet(&KeyConfig::default());
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].label(), "keydb", "no flags → keydb only");

    // --key-url only → [Online] (no keydb consulted).
    let s = build_key_sources_quiet(&KeyConfig {
        keydb_path: None,
        key_url: Some("https://8.8.8.8/keys".into()),
        key_auth: None,
    });
    assert_eq!(s.len(), 1);
    assert_eq!(
        s[0].label(),
        "online",
        "url-only first source is the online one"
    );

    // both → [Keydb, Online] — LOCAL-FIRST.
    let s = build_key_sources_quiet(&KeyConfig {
        keydb_path: Some("keydb.cfg".into()),
        key_url: Some("https://8.8.8.8/keys".into()),
        key_auth: Some("tok".into()),
    });
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].label(), "keydb", "local keydb is tried first");
    assert_eq!(s[1].label(), "online", "online service is the fallback");
}

#[test]
fn build_key_sources_drops_rejected_url_but_keeps_lan() {
    // url-only, unreachable address → rejected → zero sources.
    let s = build_key_sources_quiet(&KeyConfig {
        keydb_path: None,
        key_url: Some("https://0.0.0.0:8443/keys".into()),
        key_auth: None,
    });
    assert!(s.is_empty(), "invalid-address url must be rejected");

    // url-only, LAN/loopback https → a valid home key service.
    let s = build_key_sources_quiet(&KeyConfig {
        keydb_path: None,
        key_url: Some("https://127.0.0.1:8443/keys".into()),
        key_auth: None,
    });
    assert_eq!(s.len(), 1, "loopback url must be accepted");
    assert_eq!(s[0].label(), "online");

    // keydb + cleartext http url → only the keydb survives.
    let s = build_key_sources_quiet(&KeyConfig {
        keydb_path: Some("keydb.cfg".into()),
        key_url: Some("http://8.8.8.8/keys".into()),
        key_auth: None,
    });
    assert_eq!(s.len(), 1, "rejected url dropped; keydb remains");
    assert_eq!(s[0].label(), "keydb", "the surviving source is the keydb");
}

#[test]
fn unknown_flag_is_rejected() {
    // Regression (MEDIUM): a typo'd flag (`--titel`, `--qiet`) used to fall
    // through the catch-all and be silently ignored — defaults used, exit 0.
    // It must now be a hard error.
    assert!(parse_flags(&v(&["--titel", "1"])).is_err());
    assert!(parse_flags(&v(&["--qiet"])).is_err());
    assert!(parse_flags(&v(&["-x"])).is_err());
    // The error names the offending flag.
    let err = parse_flags(&v(&["--bogus"])).unwrap_err();
    assert!(err.contains("--bogus"), "got: {err}");
    // Non-dash positionals (URLs, title values) are NOT rejected here.
    assert!(parse_flags(&v(&["disc://", "mkv://out.mkv"])).is_ok());
    assert!(parse_flags(&v(&["-t", "1", "disc://"])).is_ok());
}

#[test]
fn boolean_flags_parse() {
    // `--log-level 2` (info) widens prose detail → verbose.
    let f = parse_flags(&v(&["--raw", "--multipass", "--log-level", "2", "-q"])).unwrap();
    assert!(f.raw && f.multipass && f.verbose && f.quiet);
    assert!(f.title_nums.is_empty());
    assert!(!f.force, "force defaults off");
}

#[test]
fn force_flag_parses() {
    // `--force` opts into overwriting a non-empty dir:// target.
    let f = parse_flags(&v(&["--force"])).unwrap();
    assert!(f.force);
    assert!(!parse_flags(&v(&[])).unwrap().force);
}

#[test]
fn keydb_does_not_take_the_following_flag_as_its_path() {
    let e = parse_flags(&v(&["--keydb", "--raw"]))
        .expect_err("a flag is not a keydb path — the value is missing");
    assert!(
        e.contains("--keydb"),
        "the error must name the flag whose value is missing: {e}"
    );
    // The real thing still parses, and still wins over nothing.
    let f = parse_flags(&v(&["--keydb", "/tmp/k.cfg", "--raw"])).unwrap();
    assert_eq!(f.keydb_path.as_deref(), Some("/tmp/k.cfg"));
    assert!(f.raw, "--raw must survive a well-formed --keydb");
}

#[test]
fn no_value_flag_takes_the_following_flag_as_its_value() {
    // Derived from VALUE_FLAGS, not hand-listed: a hand-listed version
    // omitted --log-file/--log-level, leaving the hole it closed reopened.
    // Exempts --key-auth (token may start with '-') and --key-url (own scheme check).
    for flag in crate::cli_entry::VALUE_FLAGS
        .iter()
        .copied()
        .filter(|f| !matches!(*f, "--key-auth" | "--key-url"))
    {
        // The property is that the following flag SURVIVES, not that a
        // particular error occurs: a missing value may reject or just
        // decline to consume it — but swallowing `--raw` decrypts unasked.
        match parse_flags(&v(&[flag, "--raw"])) {
            Err(e) => assert!(
                e.contains(flag),
                "{flag} rejected, but the error does not name it: {e}"
            ),
            Ok(f) => assert!(
                f.raw,
                "{flag} silently ate the following --raw, so the rip runs \
                     without it and writes a decrypted image"
            ),
        }
    }

    // ...and a well-formed value still parses, with the later flag intact.
    let f = parse_flags(&v(&["-a", "eng", "--raw"])).expect("a real value parses");
    assert!(f.raw, "--raw must survive a well-formed -a");
}

#[test]
fn log_level_sets_verbose_at_or_above_two() {
    // Level 1 = quiet prose; 2/3/4 widen it. The numeric value must also be
    // consumed so it is never mistaken for a positional URL.
    assert!(!parse_flags(&v(&["--log-level", "1"])).unwrap().verbose);
    assert!(parse_flags(&v(&["--log-level", "2"])).unwrap().verbose);
    assert!(parse_flags(&v(&["--log-level", "4"])).unwrap().verbose);
}

#[test]
fn schemeless_dest_is_unknown() {
    // Backs the `run()` guard that rejects a schemeless dest up front
    // instead of producing `name_t1.unknown` / `unknown://` outputs.
    assert!(matches!(
        libfreemkv::parse_url("out.mkv"),
        libfreemkv::StreamUrl::Unknown { .. }
    ));
    assert!(matches!(
        libfreemkv::parse_url("/path/out.mkv"),
        libfreemkv::StreamUrl::Unknown { .. }
    ));
    assert!(matches!(
        libfreemkv::parse_url("mkv://out.mkv"),
        libfreemkv::StreamUrl::Mkv { .. }
    ));
}

// Adversarial input battery: every bad-input class + combinations, each
// asserting `preflight_validate` fails loud and early (Err, never panic,
// never silent success) — the CLI maps Err to nonzero exit + no output.

/// Run `preflight_validate` on a (source, dest, raw, multipass) tuple,
/// parsing the URLs the same way `run()` does. Returns the Result so tests
/// can assert Ok / Err without repeating the parse boilerplate.
fn preflight(source: &str, dest: &str, raw: bool, multipass: bool) -> Result<(), String> {
    preflight_f(source, dest, raw, multipass, false)
}

/// `preflight` with an explicit `--force` value (for `dir://` non-empty
/// target tests).
fn preflight_f(
    source: &str,
    dest: &str,
    raw: bool,
    multipass: bool,
    force: bool,
) -> Result<(), String> {
    let ps = parse_url(source);
    let pd = parse_url(dest);
    preflight_validate(source, dest, &ps, &pd, raw, multipass, force, false)
}

/// `preflight` with title/stream selection flags marked as used.
fn preflight_sel(source: &str, dest: &str) -> Result<(), String> {
    let ps = parse_url(source);
    let pd = parse_url(dest);
    preflight_validate(source, dest, &ps, &pd, false, false, false, true)
}

#[test]
fn selection_flags_require_disc_or_iso_source() {
    // File/stream sources have no title list: -t/-a/-s must fail loud.
    assert!(preflight_sel("mkv://in.mkv", "mkv://out.mkv").is_err());
    assert!(preflight_sel("m2ts://in.m2ts", "mkv://out.mkv").is_err());
    assert!(preflight_sel("network://0.0.0.0:9000", "mkv://out.mkv").is_err());
    // disc:// IS scanned into titles: selection passes this gate. (iso://
    // shares the same is_disc_source() branch but needs a real file to
    // clear the later reachability check, so it isn't asserted here.)
    assert!(preflight_sel("disc://", &temp_dest("mkv", "sel_ok")).is_ok());
    // No selection flags: a plain file remux stays legal.
    assert!(
        preflight(
            "mkv://in.mkv",
            &temp_dest("mkv", "sel_noflags"),
            false,
            false
        )
        .is_ok()
    );
}

/// A unique temp path under the system temp dir (no tempfile dep). Caller
/// is responsible for cleanup; non-existent by construction.
pub(super) fn temp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("freemkv_test_{}_{}_{}", tag, std::process::id(), n))
}

fn temp_dest(scheme: &str, tag: &str) -> String {
    format!("{scheme}://{}", temp_path(tag).display())
}

// ── schemes ─────────────────────────────────────────────────────────────

#[test]
fn preflight_rejects_schemeless_dest() {
    let e = preflight("iso://in.iso", "out.mkv", false, false).unwrap_err();
    assert!(
        e.contains("scheme"),
        "must guide on missing dest scheme: {e}"
    );
}

#[test]
fn a_bare_container_path_is_pointed_at_its_own_scheme() {
    crate::strings::set_locale("en");
    for (bare, want) in [
        ("movie.mpg", "mpg://movie.mpg"),
        ("VTS_01_1.VOB", "mpg://VTS_01_1.VOB"),
        ("a.mkv", "mkv://a.mkv"),
        ("b.mts", "m2ts://b.mts"),
    ] {
        let e = preflight(bare, "null://", false, false).unwrap_err();
        assert!(e.contains(want), "{bare}: {e}");
        assert!(!e.contains(&format!("iso://{bare}")), "{bare}: {e}");
    }
    let e = preflight("in.iso", "null://", false, false).unwrap_err();
    assert!(e.contains("iso://in.iso"), "{e}");
}

#[test]
fn preflight_rejects_schemeless_source() {
    // A real readable ISO dest is irrelevant — the schemeless SOURCE must be
    // caught first. Use a sink dest so dest validation can't mask it.
    let e = preflight("in.iso", "null://", false, false).unwrap_err();
    assert!(
        e.to_lowercase().contains("scheme"),
        "must guide on missing source scheme: {e}"
    );
}

#[test]
fn preflight_rejects_unknown_dest_scheme() {
    // `gopher://x` parses to Unknown (no recognized scheme) → rejected.
    let e = preflight("null://", "gopher://x", false, false).unwrap_err();
    assert!(!e.is_empty());
}

// ── --raw / --multipass are iso://-only ─────────────────────────────────

#[test]
fn raw_rejected_on_mkv_dest() {
    let e = preflight("disc://", "mkv://out.mkv", true, false).unwrap_err();
    assert!(e.contains("--raw"), "names the offending flag: {e}");
    assert!(e.contains("iso://"), "points at the supported output: {e}");
}

#[test]
fn raw_rejected_on_m2ts_and_null_and_stdio() {
    for dest in ["m2ts://o.m2ts", "null://", "stdio://"] {
        let e = preflight("disc://", dest, true, false)
            .expect_err(&format!("--raw on {dest} must error"));
        assert!(e.contains("--raw"), "{dest}: {e}");
    }
}

#[test]
fn multipass_rejected_on_mkv_dest() {
    let e = preflight("disc://", "mkv://out.mkv", false, true).unwrap_err();
    assert!(e.contains("--multipass"), "names the flag: {e}");
    assert!(e.contains("iso://"), "points at iso://: {e}");
}

#[test]
fn multipass_rejected_on_null_and_stdio_and_network() {
    for dest in ["null://", "stdio://", "network://host:9000"] {
        let e = preflight("disc://", dest, false, true)
            .expect_err(&format!("--multipass on {dest} must error"));
        assert!(e.contains("--multipass"), "{dest}: {e}");
    }
}

#[test]
fn disc_to_mkv_raw_combination_errors() {
    // disc→mkv --raw: the explicit combination called out in the brief.
    assert!(preflight("disc://", "mkv://o.mkv", true, false).is_err());
}

#[test]
fn disc_to_mkv_multipass_combination_errors() {
    assert!(preflight("disc://", "mkv://o.mkv", false, true).is_err());
}

#[test]
fn disc_to_null_raw_and_multipass_error() {
    // disc→null --raw and disc→null --multipass: both error (iso://-only).
    assert!(preflight("disc://", "null://", true, false).is_err());
    assert!(preflight("disc://", "null://", false, true).is_err());
}

// ── dir:// (decrypted file-tree extraction) gates ───────────────────────

/// `--raw` into a `dir://` dest is rejected (dir:// is not iso://, so the
/// system-wide raw/iso-only gate fires). An encrypted file tree is useless.
#[test]
fn dir_dest_rejects_raw() {
    let out = temp_path("dir_raw");
    let dest = format!("dir://{}/", out.display());
    let e = preflight("disc://", &dest, true, false).expect_err("dir:// + --raw must error");
    assert!(e.contains("--raw"), "names the offending flag: {e}");
    let _ = std::fs::remove_dir_all(&out);
}

/// `--multipass` into a `dir://` dest is rejected (dir:// is 1-shot;
/// recovery is the iso:// multipass path's job).
#[test]
fn dir_dest_rejects_multipass() {
    let out = temp_path("dir_mp");
    let dest = format!("dir://{}/", out.display());
    let e = preflight("disc://", &dest, false, true).expect_err("dir:// + --multipass must error");
    assert!(e.contains("--multipass"), "names the flag: {e}");
    let _ = std::fs::remove_dir_all(&out);
}

/// A byte-stream source (no filesystem) into `dir://` is rejected up front
/// — only disc:// / iso:// supply a UDF tree.
#[test]
fn dir_dest_rejects_byte_stream_source() {
    for src in [
        "mkv://in.mkv",
        "m2ts://in.m2ts",
        "network://host:9000",
        "stdio://",
    ] {
        let out = temp_path("dir_src");
        let dest = format!("dir://{}/", out.display());
        let e =
            preflight(src, &dest, false, false).expect_err(&format!("{src} → dir:// should error"));
        assert!(
            e.to_lowercase().contains("dir://") || e.contains("file tree"),
            "{src}: {e}"
        );
        let _ = std::fs::remove_dir_all(&out);
    }
}

/// disc:// and iso:// SOURCES into dir:// pass the source gate (an iso://
/// input still needs a readable file, supplied here).
#[test]
fn dir_dest_accepts_disc_and_iso_sources() {
    let out = temp_path("dir_ok");
    let dest = format!("dir://{}/", out.display());
    // disc:// (auto-detect device): source gate passes; dir target created.
    assert!(preflight("disc://", &dest, false, false).is_ok());
    let _ = std::fs::remove_dir_all(&out);

    // iso:// source needs a real, non-empty file.
    let iso = temp_path("dir_ok_iso");
    std::fs::write(&iso, b"not empty").unwrap();
    let out2 = temp_path("dir_ok2");
    let dest2 = format!("dir://{}/", out2.display());
    let src = format!("iso://{}", iso.display());
    assert!(preflight(&src, &dest2, false, false).is_ok());
    let _ = std::fs::remove_file(&iso);
    let _ = std::fs::remove_dir_all(&out2);
}

/// A non-empty `dir://` target is refused without `--force`, accepted with.
#[test]
fn dir_dest_non_empty_requires_force() {
    let out = temp_path("dir_nonempty");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("x.txt"), b"x").unwrap();
    let dest = format!("dir://{}/", out.display());
    let e = preflight_f("disc://", &dest, false, false, false).expect_err("non-empty must error");
    assert!(e.to_lowercase().contains("empty"), "{e}");
    // --force overrides.
    assert!(preflight_f("disc://", &dest, false, false, true).is_ok());
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn dir_dest_existing_file_rejected_even_with_force() {
    let f = temp_path("dir_isfile");
    std::fs::write(&f, b"i am a file, not a folder").unwrap();
    let dest = format!("dir://{}", f.display());

    // Without --force.
    let e = preflight("disc://", &dest, false, false)
        .expect_err("dir:// target that is a file must error");
    assert!(
        e.to_lowercase().contains("file") || e.to_lowercase().contains("folder"),
        "must explain the file/folder mismatch: {e}"
    );

    // --force must NOT rescue it — a regular file is still not a folder.
    let e2 = preflight_f("disc://", &dest, false, false, true)
        .expect_err("dir:// target that is a file must error even with --force");
    assert!(
        e2.to_lowercase().contains("file") || e2.to_lowercase().contains("folder"),
        "--force must not turn a file into a dir:// target: {e2}"
    );
    let _ = std::fs::remove_file(&f);
}

#[test]
fn raw_and_multipass_accepted_on_iso_dest() {
    // The legit case: iso:// destination accepts both flags. (Source is the
    // live drive, not pre-checked for existence here — device None.)
    assert!(preflight("disc://", &temp_dest("iso", "raw"), true, false).is_ok());
    assert!(preflight("disc://", &temp_dest("iso", "multipass"), false, true).is_ok());
    assert!(preflight("disc://", &temp_dest("iso", "both"), true, true).is_ok());
}

#[test]
fn no_flags_accepted_on_non_iso_dest() {
    // Without the iso-only flags, a mux/sink dest is fine at preflight.
    assert!(preflight("disc://", &temp_dest("mkv", "noflags"), false, false).is_ok());
    assert!(preflight("disc://", "null://", false, false).is_ok());
}

// ── drive / device source ────────────────────────────────────────────────

#[test]
fn missing_device_path_errors_early() {
    // An explicit device path that doesn't exist must be caught before any
    // open. Use a sink dest so only the source check can fire.
    let e = preflight("disc:///dev/does-not-exist-xyz", "null://", false, false).unwrap_err();
    assert!(
        e.to_lowercase().contains("device") || e.contains("does-not-exist"),
        "must name the missing device: {e}"
    );
}

#[test]
fn auto_detect_device_not_prechecked() {
    // `disc://` with no device path is auto-detect — left to find_drive, so
    // preflight must NOT error on it for source reachability.
    assert!(preflight("disc://", "null://", false, false).is_ok());
}

// ── ISO input ────────────────────────────────────────────────────────────

#[test]
fn iso_input_missing_errors() {
    let p = temp_path("nope.iso");
    let e = validate_iso_input(&p).unwrap_err();
    assert!(e.to_lowercase().contains("not found"), "{e}");
}

#[test]
fn iso_input_directory_errors() {
    let dir = temp_path("isodir");
    std::fs::create_dir(&dir).unwrap();
    let e = validate_iso_input(&dir).unwrap_err();
    let _ = std::fs::remove_dir(&dir);
    assert!(e.to_lowercase().contains("directory"), "{e}");
}

#[test]
fn iso_input_empty_errors() {
    let f = temp_path("empty.iso");
    std::fs::write(&f, b"").unwrap();
    let e = validate_iso_input(&f).unwrap_err();
    let _ = std::fs::remove_file(&f);
    assert!(e.to_lowercase().contains("empty"), "{e}");
}

#[test]
fn iso_input_nonempty_file_passes_cheap_check() {
    // A non-empty readable file passes the CHEAP preflight (deep image
    // validity is the scan's job, not preflight's).
    let f = temp_path("ok.iso");
    std::fs::write(&f, vec![0u8; 4096]).unwrap();
    let r = validate_iso_input(&f);
    let _ = std::fs::remove_file(&f);
    assert!(r.is_ok(), "non-empty file must pass cheap iso check: {r:?}");
}

#[test]
fn iso_source_missing_errors_through_preflight() {
    // Full path: an iso:// source pointing at a missing file errors in
    // preflight (not just the unit helper).
    let p = temp_path("missing.iso");
    let src = format!("iso://{}", p.display());
    let e = preflight(&src, "null://", false, false).unwrap_err();
    assert!(e.to_lowercase().contains("not found"), "{e}");
}

// ── output destination ───────────────────────────────────────────────────

/// mpg:// is a single-file destination like mkv:// and mp4:// (design §1.1): preflight
/// refuses an unwritable one with the file-dest message, before any work.
#[test]
fn preflight_checks_every_file_container_dest() {
    let src = temp_path("pf_src.iso");
    std::fs::write(&src, vec![0u8; 2048 * 32]).unwrap();
    let missing_dir = temp_path("pf_no_such_dir");
    for scheme in ["mkv", "mp4", "mpg", "m2ts"] {
        let dest = missing_dir.join(format!("movie.{scheme}"));
        let want = validate_file_dest(&dest).unwrap_err();
        let got = preflight(
            &format!("iso://{}", src.display()),
            &format!("{scheme}://{}", dest.display()),
            false,
            false,
        );
        assert_eq!(got, Err(want), "{scheme}:// dest");
    }
    let _ = std::fs::remove_file(&src);
}

#[test]
fn dest_parent_missing_errors() {
    // mkv:// whose parent directory does not exist must error before work.
    let missing_dir = temp_path("no_such_dir");
    let dest = missing_dir.join("movie.mkv");
    let e = validate_file_dest(&dest).unwrap_err();
    assert!(
        e.to_lowercase().contains("director") || e.to_lowercase().contains("exist"),
        "{e}"
    );
}

#[test]
fn dest_is_existing_directory_errors() {
    // A path that is an existing DIRECTORY can't receive a single-file write.
    let dir = temp_path("existing_dir");
    std::fs::create_dir(&dir).unwrap();
    let e = validate_file_dest(&dir).unwrap_err();
    let _ = std::fs::remove_dir(&dir);
    assert!(e.to_lowercase().contains("director"), "{e}");
}

#[test]
fn dest_writable_parent_passes_and_leaves_no_probe_file() {
    // A writable parent + non-existent target passes, and the writability
    // probe must NOT leave its temp file behind.
    let f = temp_path("writable.mkv");
    let r = validate_file_dest(&f);
    assert!(r.is_ok(), "writable dest must pass: {r:?}");
    assert!(
        !f.exists(),
        "the writability probe must clean up its temp file"
    );
}

#[test]
fn dest_writable_check_does_not_truncate_existing_file() {
    // If the target already exists, the probe must NOT truncate it (we open
    // append, not create-new). Pre-seed content and assert it survives.
    let f = temp_path("preexisting.mkv");
    std::fs::write(&f, b"keepme").unwrap();
    let r = validate_file_dest(&f);
    let survived = std::fs::read(&f).unwrap_or_default();
    let _ = std::fs::remove_file(&f);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(survived, b"keepme", "existing output must not be truncated");
}

#[test]
fn full_preflight_dest_parent_missing_errors() {
    let missing = temp_path("nodir");
    let dest = format!("mkv://{}", missing.join("m.mkv").display());
    let e = preflight("null://", &dest, false, false).unwrap_err();
    assert!(!e.is_empty());
}

// ── predicates ───────────────────────────────────────────────────────────

#[test]
fn raw_and_multipass_require_a_drive_source() {
    assert!(preflight("iso://in.iso", "iso://out.iso", false, true).is_err());
    assert!(preflight("iso://in.iso", "iso://out.iso", true, false).is_err());
    // A drive source still accepts both.
    assert!(preflight("disc://", "iso://out.iso", true, false).is_ok());
    assert!(preflight("disc://", "iso://out.iso", false, true).is_ok());
    // And they remain rejected for a mux destination, as before.
    assert!(preflight("disc://", "mkv://out.mkv", false, true).is_err());
}

#[test]
fn scheme_only_sink_predicate() {
    assert!(is_scheme_only_sink(&parse_url("null://")));
    assert!(is_scheme_only_sink(&parse_url("stdio://")));
    assert!(!is_scheme_only_sink(&parse_url("mkv://x.mkv")));
    assert!(!is_scheme_only_sink(&parse_url("iso://x.iso")));
}

// ── null:// multi-title routing fix ──────────────────────────────────────

#[test]
fn null_dest_multi_title_routes_all_to_sink() {
    let titles = Some(vec![
        libfreemkv::DiscTitle::empty(),
        libfreemkv::DiscTitle::empty(),
        libfreemkv::DiscTitle::empty(),
    ]);
    let out = Output::new(false, true);
    let parsed = parse_url("null://");
    let jobs = build_jobs(&titles, false, &[], false, "null://", &parsed, &out)
        .expect("null:// multi-title must build jobs, not fail");
    assert_eq!(jobs.len(), 3, "one job per title");
    for (idx, url) in &jobs {
        assert!(idx.is_some(), "each job names its title index");
        assert_eq!(url, "null://", "every title routes to the bare sink");
        assert!(
            matches!(parse_url(url), libfreemkv::StreamUrl::Null),
            "the sink URL must re-parse to Null (not Unknown): {url}"
        );
    }
}

/// `stdio://` (the other scheme-only sink) gets the same multi-title routing.
#[test]
fn stdio_dest_multi_title_routes_all_to_sink() {
    let titles = Some(vec![
        libfreemkv::DiscTitle::empty(),
        libfreemkv::DiscTitle::empty(),
    ]);
    let out = Output::new(false, true);
    let parsed = parse_url("stdio://");
    let jobs = build_jobs(&titles, false, &[], false, "stdio://", &parsed, &out)
        .expect("stdio:// multi-title must build jobs");
    assert_eq!(jobs.len(), 2);
    for (_idx, url) in &jobs {
        assert_eq!(url, "stdio://");
    }
}

#[test]
fn demux_dest_multi_title_urls_carry_scheme() {
    let titles = Some(vec![
        libfreemkv::DiscTitle::empty(),
        libfreemkv::DiscTitle::empty(),
    ]);
    let out = Output::new(false, true);
    let parsed = parse_url("demux://out/");
    let jobs = build_jobs(&titles, false, &[], false, "demux://out/", &parsed, &out)
        .expect("demux:// multi-title must build jobs");
    assert_eq!(jobs.len(), 2, "one job per title");
    // t01 / t02 subdirs, each a valid Demux URL (scheme present).
    assert_eq!(jobs[0].1, "demux://out/t01/");
    assert_eq!(jobs[1].1, "demux://out/t02/");
    for (idx, url) in &jobs {
        assert!(idx.is_some(), "each job names its title index");
        assert!(
            matches!(parse_url(url), libfreemkv::StreamUrl::Demux { .. }),
            "the job URL must re-parse to Demux (not Unknown): {url}"
        );
    }
}

#[test]
fn kind_filter_dest_multi_title_urls_carry_own_scheme() {
    for scheme in ["video", "audio", "sub"] {
        let titles = Some(vec![
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
        ]);
        let out = Output::new(false, true);
        let dest = format!("{scheme}://out/");
        let parsed = parse_url(&dest);
        let jobs = build_jobs(&titles, false, &[], false, &dest, &parsed, &out)
            .unwrap_or_else(|| panic!("{scheme}:// multi-title must build jobs"));
        assert_eq!(jobs.len(), 2, "{scheme}: one job per title");
        assert_eq!(jobs[0].1, format!("{scheme}://out/t01/"), "{scheme} t01");
        assert_eq!(jobs[1].1, format!("{scheme}://out/t02/"), "{scheme} t02");
        for (idx, url) in &jobs {
            assert!(idx.is_some(), "{scheme}: each job names its title index");
            // Must re-parse to its OWN kind, not Demux and not Unknown — this
            // is what preserves the kind filter through multi-title fan-out.
            let reparsed = parse_url(url);
            let ok = match scheme {
                "video" => matches!(reparsed, libfreemkv::StreamUrl::Video { .. }),
                "audio" => matches!(reparsed, libfreemkv::StreamUrl::Audio { .. }),
                "sub" => matches!(reparsed, libfreemkv::StreamUrl::Sub { .. }),
                _ => unreachable!(),
            };
            assert!(ok, "{scheme}: job URL must re-parse to its own kind: {url}");
        }
    }
}

/// A real file dest (mkv://) on a multi-title source still routes through
/// per-title naming (the sink special-case must NOT swallow file dests).
#[test]
fn file_dest_multi_title_still_named_per_title() {
    let mut t0 = libfreemkv::DiscTitle::empty();
    t0.playlist = "Movie".into();
    let titles = Some(vec![t0, libfreemkv::DiscTitle::empty()]);
    let out = Output::new(false, true);
    // Directory dest (trailing slash) → one named file per title.
    let dir = temp_path("mkvout");
    let dest = format!("{}/", dir.display());
    let parsed = parse_url(&format!("mkv://{}/", dir.display()));
    let jobs = build_jobs(&titles, false, &[], true, &dest, &parsed, &out);
    let _ = std::fs::remove_dir_all(&dir);
    let jobs = jobs.expect("dir dest builds per-title jobs");
    assert_eq!(jobs.len(), 2);
    for (_idx, url) in &jobs {
        assert!(url.contains("_t"), "per-title file naming preserved: {url}");
    }
}

/// `preflight_validate` must NEVER panic, on any combination of adversarial
/// scheme strings × flag states. The only acceptable outcomes are Ok or Err
/// — a panic here would crash the CLI on malformed input.
#[test]
fn preflight_never_panics_on_adversarial_combinations() {
    let urls = [
        "",
        "://",
        "disc://",
        "disc:///dev/null",
        "iso://",
        "iso://\0",
        "mkv://",
        "m2ts://x",
        "null://",
        "null://trailing",
        "stdio://",
        "network://",
        "network://host:9000",
        "gopher://x",
        "out.mkv",
        "/abs/path",
        "iso://日本語.iso",
        &"iso://".to_string().repeat(1000),
    ];
    for &s in &urls {
        for &d in &urls {
            for raw in [false, true] {
                for mp in [false, true] {
                    // Must return (Ok or Err), never panic.
                    let _ = preflight(s, d, raw, mp);
                }
            }
        }
    }
}

// WS3 — dropped `-k` short flag (rc.6: `--keydb` long form only). Must be
// an unknown-flag hard error, not silently consume its value, or a user
// on the old flag gets a confusing error, or the rip proceeds against the default keydb.

/// `-k <path>` is no longer a recognized flag: it must be rejected as an
/// unknown flag (so the user is told to use `--keydb`), never quietly
/// accepted. The long `--keydb` form (tested elsewhere) is the only spelling.
#[test]
fn dropped_short_k_flag_is_unknown() {
    // `-k keydb.cfg` — the dropped short form — must error.
    let err = parse_flags(&v(&["-k", "keydb.cfg"])).unwrap_err();
    assert!(
        err.contains("-k"),
        "unknown-flag error must name `-k`: {err}"
    );
    // It must NOT have been parsed as a keydb path (the parse failed, so no
    // ParsedFlags exists — but assert the long form still works to prove we
    // didn't break `--keydb` while dropping `-k`).
    let f = parse_flags(&v(&["--keydb", "keydb.cfg"])).unwrap();
    assert_eq!(f.keydb_path.as_deref(), Some("keydb.cfg"));
    // Bare `-k` (no value) is likewise unknown, not "needs a value".
    let err = parse_flags(&v(&["-k"])).unwrap_err();
    assert!(
        err.contains("-k") && !err.contains("requires a value"),
        "bare `-k` must be unknown-flag, not flag_needs_value: {err}"
    );
}

#[test]
fn dropped_device_flags_are_unknown() {
    for bad in [
        v(&["--device", "/dev/sg0"]),
        v(&["-d", "/dev/sg0"]),
        v(&["--device"]),
        v(&["-d"]),
    ] {
        match parse_flags(&bad) {
            Ok(f) => panic!("{bad:?} must be rejected, got {f:?}"),
            Err(err) => assert!(!err.is_empty(), "{bad:?}: unknown-flag error must be set"),
        }
    }
}

#[test]
fn value_flag_set_matches_parser() {
    // The arity table, written out rather than read back from the code
    // under test. `-k` is deliberately NOT here: it is a retired flag, and
    // `dropped_short_k_flag_is_unknown` (above) pins the rejection.
    const EXPECTED: &[&str] = &[
        "-t",
        "--title",
        "-a",
        "--audio",
        "-s",
        "--subtitles",
        "--keydb",
        "--key-url",
        "--key-auth",
        "--log-file",
        "--log-level",
    ];
    assert_eq!(
        crate::cli_entry::VALUE_FLAGS,
        EXPECTED,
        "VALUE_FLAGS drifted from the parser's value-taking flags; teach \
             `parse_flags` the new flag (or drop it here) before editing this list"
    );

    // A token no parser arm names, so it can only ever be swallowed as some
    // flag's value or rejected as unknown — never both.
    const SENTINEL: &str = "--freemkv-no-such-flag";
    // The English text of `error.unknown_flag`; the tests run under the
    // default (en) locale, as `dropped_short_k_flag_is_unknown` does.
    const UNKNOWN: &str = "unknown flag";

    for flag in crate::cli_entry::VALUE_FLAGS {
        if let Err(e) = parse_flags(&v(&[flag])) {
            assert!(
                !e.contains(UNKNOWN),
                "`{flag}` is in VALUE_FLAGS but `parse_flags` does not know it: {e}"
            );
        }
        // The sentinel is flag-shaped, so this probe measures the flag-guard,
        // not arity, for flags refusing a flag-shaped value (the logging
        // pair) — their arity is covered by a dedicated test instead.
        if matches!(*flag, "--log-file" | "--log-level") {
            continue;
        }
        if let Err(e) = parse_flags(&v(&[flag, SENTINEL])) {
            assert!(
                !e.contains(UNKNOWN),
                "`{flag}` must consume `{SENTINEL}` as its value, not leave \
                     it to be parsed as a flag: {e}"
            );
        }
    }

    // The other side of the contract: a boolean flag must NOT be listed,
    // and must leave the next token alone.
    for flag in ["-q", "--quiet", "--raw", "--multipass", "--force"] {
        assert!(
            !crate::cli_entry::VALUE_FLAGS.contains(&flag),
            "`{flag}` takes no value; listing it makes `collect_urls` eat \
                 the next token"
        );
        let e = parse_flags(&v(&[flag, SENTINEL]))
            .expect_err("the sentinel must reach the unknown-flag arm");
        assert!(
            e.contains(UNKNOWN) && e.contains(SENTINEL),
            "`{flag}` swallowed the following token: {e}"
        );
    }
}

#[test]
fn retired_value_flags_are_rejected_by_the_parser() {
    for flag in crate::cli_entry::RETIRED_VALUE_FLAGS {
        assert!(
            !crate::cli_entry::VALUE_FLAGS.contains(flag),
            "`{flag}` cannot be both live and retired"
        );
        let err = parse_flags(&v(&[flag, "value"]))
            .expect_err("a retired flag must be rejected, not parsed");
        assert!(
            err.contains("unknown flag") && err.contains(flag),
            "the rejection of `{flag}` must name it: {err}"
        );
    }
}

// WS3 — device comes from the source URL (`disc:///dev/sgN`), not a
// `--device` flag. Pins the exact `parse_url` shape that `info_cmd` /
// `pipe_disc` / `dir_to_extract` depend on to read the device out.

#[test]
fn device_comes_from_disc_url() {
    // Explicit device path → carried in the URL.
    match parse_url("disc:///dev/sg3") {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => {
            assert_eq!(p.to_string_lossy(), "/dev/sg3");
        }
        other => panic!("disc:///dev/sg3 must parse to Disc{{device:Some}}, got {other:?}"),
    }
    // Bare `disc://` → auto-detect (device None); the routes fall back to
    // `find_drive()` rather than a flag.
    match parse_url("disc://") {
        libfreemkv::StreamUrl::Disc { device: None } => {}
        other => panic!("disc:// must parse to Disc{{device:None}}, got {other:?}"),
    }
    // A Windows device path survives the URL too (the route is OS-agnostic;
    // the path string is opaque to the parser).
    match parse_url("disc://D:") {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => {
            assert_eq!(p.to_string_lossy(), "D:");
        }
        other => panic!("disc://D: must parse to Disc{{device:Some}}, got {other:?}"),
    }
}

// WS3 — path handling. `sanitize_name` seeds per-title output filenames;
// it must never emit a path separator, illegal character, or empty stem
// on any platform, since a rip authored on Linux may be muxed on Windows.

#[test]
fn sanitize_name_strips_path_separators_and_illegal_chars() {
    // Forward AND back slashes must not survive, or a `Movie/Part2` stem
    // synthesizes `dir/Movie/Part2_t1.mkv`, escaping the dest dir. The
    // separator is stripped (not converted), so space-only gaps become underscores.
    let s = sanitize_name("Movie/Part 2");
    assert!(!s.contains('/'), "path separator survived: {s}");
    assert_eq!(s, "MoviePart_2", "got {s}");
    let s = sanitize_name(r"A\B");
    assert!(!s.contains('\\'), "backslash survived: {s}");
    assert_eq!(s, "AB", "backslash stripped, not converted: {s}");
    // Windows-illegal punctuation (`: * ? " < > |`) is dropped, not kept.
    let s = sanitize_name(r#"a:b*c?d"e<f>g|h"#);
    for bad in [':', '*', '?', '"', '<', '>', '|', '\\', '/'] {
        assert!(!s.contains(bad), "illegal char {bad:?} survived in {s}");
    }
    assert_eq!(s, "abcdefgh", "got {s}");
}

#[test]
fn sanitize_name_spaces_to_underscores_and_trims() {
    assert_eq!(sanitize_name("  The  Movie  "), "The__Movie");
    // Hyphen and underscore are preserved (legal everywhere).
    assert_eq!(sanitize_name("Director-Cut_2"), "Director-Cut_2");
}

#[test]
fn sanitize_name_empty_or_all_illegal_falls_back_to_disc() {
    // An empty or fully-stripped stem must fall back to "disc" so the
    // per-title filename is never `_t1.mkv` (leading underscore, no stem).
    assert_eq!(sanitize_name(""), "disc");
    assert_eq!(sanitize_name("///"), "disc");
    assert_eq!(sanitize_name(":*?"), "disc");
    assert_eq!(sanitize_name("   "), "disc");
    // A name that is ALL non-ascii is stripped to empty → "disc".
    assert_eq!(sanitize_name("日本語"), "disc");
}

// WS3 — keydb path resolution. `resolved_keydb_path` honors an explicit
// `--keydb` override, else falls back to exe-local/default, and never
// panics. The search policy itself lives in `freemkv-keysources::paths`.

#[test]
fn resolved_keydb_path_honors_explicit_override() {
    // An explicit `--keydb PATH` is used verbatim, never the search policy.
    let p = resolved_keydb_path(&Some("/custom/keydb.cfg".to_string()));
    assert_eq!(p, std::path::PathBuf::from("/custom/keydb.cfg"));
    // A Windows-style override path is passed through unchanged too.
    let p = resolved_keydb_path(&Some(r"C:\keys\keydb.cfg".to_string()));
    assert_eq!(p, std::path::PathBuf::from(r"C:\keys\keydb.cfg"));
}

#[test]
fn resolved_keydb_path_falls_back_without_panicking() {
    // No override → the exe-local/default policy (or the bare `keydb.cfg`
    // last resort). Either way a non-empty path is returned, never a panic.
    let p = resolved_keydb_path(&None);
    assert!(
        p.file_name().is_some_and(|n| n == "keydb.cfg"),
        "fallback must end in keydb.cfg: {}",
        p.display()
    );
}

// WS3 — dir:// preflight messaging render. Pins that the rendered message
// substitutes placeholders (no leftover `{path}`/`{source}`); the dir://
// gate tests above cover which inputs error, this covers the text.

#[test]
fn dir_source_unsupported_message_substitutes_and_guides() {
    // A byte-stream source into dir:// renders the localized guidance with
    // the offending source URL substituted and no leftover placeholder.
    let out = temp_path("dir_msg_src");
    let dest = format!("dir://{}/", out.display());
    let err = preflight("mkv://in.mkv", &dest, false, false).unwrap_err();
    let _ = std::fs::remove_dir_all(&out);
    assert!(
        err.contains("mkv://in.mkv"),
        "source not substituted: {err}"
    );
    assert!(!err.contains("{source}"), "leftover placeholder: {err}");
    // Guides toward a usable source (disc:// or iso://).
    assert!(
        err.contains("disc://") || err.contains("iso://"),
        "must guide to a filesystem source: {err}"
    );
}

#[test]
fn dir_dest_is_file_message_substitutes_path() {
    // A dir:// target that is a regular file renders the path-substituted
    // file/folder mismatch message with no leftover `{path}`.
    let f = temp_path("dir_msg_file");
    std::fs::write(&f, b"x").unwrap();
    let dest = format!("dir://{}", f.display());
    let err = preflight("disc://", &dest, false, false).unwrap_err();
    let _ = std::fs::remove_file(&f);
    assert!(
        err.contains(&f.display().to_string()),
        "path not substituted: {err}"
    );
    assert!(!err.contains("{path}"), "leftover placeholder: {err}");
}

// WS3 — the WS2 messaging render shape: `main::fatal` builds the fatal
// block from `error.fatal_header` and `error.fatal_diagnostic_hint`.
// Pins the shape so a locale/template drop of the code or level is caught.

#[test]
fn fatal_header_assembles_level_op_and_code_forward_cause() {
    // The cause fragment for a real library error, code-forward (E-prefixed).
    let cause = fmt_err(&"E6009");
    assert!(
        cause.starts_with("E6009 "),
        "cause not code-forward: {cause}"
    );

    // The render site assembles `{level}: {op} failed: {cause}`. Reproduce
    // the exact substitution `main::fatal` performs (it isn't callable — it
    // exits the process — so we pin the template + parts it feeds).
    let level = crate::strings::get(crate::messaging::Level::Error.locale_key());
    let op = crate::strings::get("error.op_rip");
    let header = crate::strings::fmt(
        "error.fatal_header",
        &[("level", &level), ("op", &op), ("cause", &cause)],
    );
    // All three parts present, in order, with no leftover placeholders.
    assert!(
        header.starts_with(&format!("{level}:")),
        "level first: {header}"
    );
    assert!(header.contains(&op), "op missing: {header}");
    assert!(header.contains(&cause), "cause missing: {header}");
    assert!(
        !header.contains("{level}") && !header.contains("{op}") && !header.contains("{cause}"),
        "leftover placeholder in fatal header: {header}"
    );
    // The diagnostic-log hint exists and names the --log-level escape hatch.
    let hint = crate::strings::get("error.fatal_diagnostic_hint");
    assert_ne!(hint, "error.fatal_diagnostic_hint", "hint key missing");
    assert!(
        hint.contains("--log-level"),
        "hint must point at the log flag: {hint}"
    );
}

#[test]
fn fatal_operation_keys_all_resolve() {
    for key in [
        "error.op_rip",
        "error.op_info",
        "error.op_verify",
        "error.op_update_keys",
    ] {
        assert_ne!(
            crate::strings::get(key),
            key,
            "fatal op key {key} unresolved (would print the raw key)"
        );
    }
}

// WS3 — Windows-only, compile-gated. Doesn't run in the Mac precommit but
// must compile cleanly so Windows CI validates it. Pins the OS-specific
// path/keydb shapes the CLI relies on under Windows.

#[cfg(windows)]
#[test]
fn windows_keydb_override_keeps_drive_letter_path() {
    // A Windows override path (drive letter + backslashes) must survive
    // verbatim through the CLI wrapper, unmangled.
    let p = resolved_keydb_path(&Some(r"C:\Users\me\AppData\keydb.cfg".to_string()));
    assert_eq!(
        p,
        std::path::PathBuf::from(r"C:\Users\me\AppData\keydb.cfg")
    );
}

#[cfg(windows)]
#[test]
fn windows_sanitize_name_drops_reserved_punctuation() {
    // `:` (drive separator) and `\` must never reach a synthesized
    // filename. Pinned under the Windows build so CI's Windows job
    // catches a regression even if this later becomes cfg-specific.
    let s = sanitize_name(r"C:\Movie");
    assert!(!s.contains(':') && !s.contains('\\'), "got {s}");
}
