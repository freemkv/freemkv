use super::{
    CopyVerdict, DISC_COPY_DAMAGED_EXIT, PipeFail, check_selection_coverage, copy_verdict,
    disc_copy_exit_code, extract_succeeded, finalize_mux, title_policy,
};
use crate::output::Output;

fn outcome(completed: bool, bytes: u64) -> libfreemkv::MuxOutcome {
    libfreemkv::MuxOutcome {
        completed,
        halted: false,
        output_opened: true,
        bytes_written: bytes,
        errors: 0,
        lost_bytes: 0,
        streams: 3,
        undelivered_streams: vec![],
    }
}

fn quiet() -> Output {
    Output::new(false, true)
}

#[test]
fn a_completed_but_lossy_mux_names_the_undelivered_stream() {
    // Pin the catalog so the assertion reads the English wording regardless of
    // the machine's LANG.
    crate::strings::set_locale("en");

    let mut lossy = outcome(true, 1_000_000);
    lossy.undelivered_streams = vec![1];
    let events = super::CliMuxEvents::new(loud(), "mp4:///out/x.mp4".into(), false);

    let (res, printed) = crate::output::capture(|| finalize_mux(Ok(lossy), &loud(), &events));

    // Still a success: the file is finalized and playable, just missing a track.
    assert!(
        res.is_ok(),
        "a completed mux missing one track is not a truncated file"
    );
    assert!(
        printed.contains("Track 2"),
        "the undelivered stream must be NAMED in the output the user sees; got:\n{printed}"
    );
    assert!(
        printed.contains("left out"),
        "the user must be told the file is missing tracks; got:\n{printed}"
    );

    // The clean case must stay clean — no phantom warning on a lossless rip.
    let events = super::CliMuxEvents::new(loud(), "mp4:///out/x.mp4".into(), false);
    let (ok, clean) =
        crate::output::capture(|| finalize_mux(Ok(outcome(true, 1_000_000)), &loud(), &events));
    assert!(ok.is_ok());
    assert!(
        !clean.contains("left out"),
        "a lossless mux must not warn; got:\n{clean}"
    );
}

#[test]
fn a_completed_mux_that_dropped_payload_bytes_says_how_much_it_lost() {
    crate::strings::set_locale("en");

    let mut holed = outcome(true, 1_000_000);
    holed.errors = 2;
    holed.lost_bytes = 3 << 20;
    let events = super::CliMuxEvents::new(loud(), "mkv:///out/movie.mkv".into(), false);

    let (res, printed) = crate::output::capture(|| finalize_mux(Ok(holed), &loud(), &events));

    // Still a success for the exit code: the file is finalised and
    // playable, and the per-title loop must not abandon a batch over it —
    // the same call the sibling `undelivered_streams` warning makes.
    assert!(res.is_ok());
    assert!(
        printed.contains("lost"),
        "the loss must be in the output the user sees; got:\n{printed}"
    );
    assert!(
        printed.contains('3'),
        "and it must say HOW MUCH was lost; got:\n{printed}"
    );

    // A lossless mux stays silent: an unconditional warning is worse than
    // none at all.
    let events = super::CliMuxEvents::new(loud(), "mkv:///out/movie.mkv".into(), false);
    let (ok, clean) =
        crate::output::capture(|| finalize_mux(Ok(outcome(true, 1_000_000)), &loud(), &events));
    assert!(ok.is_ok());
    assert!(
        !clean.contains("lost"),
        "a lossless mux must not warn; got:\n{clean}"
    );
}

/// Normal verbosity — the level a user gets by default, and the only one at
/// which `Level::Normal` lines are observable.
fn loud() -> Output {
    Output::new(false, false)
}

// A failed title prints its error straight after the source's `OK`, and the blank
// that closes the title after the error.
#[test]
fn a_failed_title_prints_its_error_before_the_closing_blank() {
    let dir = crate::ku_fixtures::TempDir::new("title-blank");
    let dest = format!("mkv://{}", dir.path().join("out.mkv").display());
    let (code, printed) = crate::output::capture(|| super::run("null://", &dest, &[]));
    assert_eq!(code, 1, "{printed}");
    let error = printed
        .lines()
        .position(|l| l.contains("E9001"))
        .unwrap_or_else(|| panic!("no E9001 line: {printed:?}"));
    let lines: Vec<&str> = printed.lines().collect();
    assert!(
        lines[error - 1].ends_with(&crate::strings::get("rip.ok")),
        "the error must follow the open notice: {printed:?}"
    );
    assert_eq!(lines.get(error + 1), Some(&""), "{printed:?}");
}

#[test]
fn a_truncated_mux_is_a_failure_not_a_success() {
    let events = super::CliMuxEvents::new(quiet(), "mkv:///out/x.mkv".into(), true);

    let good = finalize_mux(Ok(outcome(true, 1_000_000)), &quiet(), &events);
    assert!(good.is_ok(), "a completed mux is a success");

    let truncated = finalize_mux(Ok(outcome(false, 1_000)), &quiet(), &events)
        .expect_err("a mux that did not complete left a truncated file — never Ok");
    // Halted, not Failed: the multi-title loop must FULL-STOP rather than
    // cancel each remaining title one at a time.
    assert_eq!(truncated.result, freemkv_engine::TitleResult::Halted);

    // Bytes written is not the test — a halt after 5 GB is still a halt.
    let big = finalize_mux(Ok(outcome(false, 5_000_000_000)), &quiet(), &events);
    assert!(
        big.is_err(),
        "bytes written cannot excuse an incomplete mux"
    );

    // A hard I/O error is classified through the mux path, not as a halt.
    let failed = finalize_mux(Err(std::io::Error::other("E7022")), &quiet(), &events)
        .expect_err("an errored mux is a failure");
    assert_ne!(failed.result, freemkv_engine::TitleResult::Halted);
}

fn copy_result(halted: bool, good: u64, unreadable: u64) -> freemkv_engine::CopyResult {
    freemkv_engine::CopyResult {
        bytes_total: good + unreadable,
        bytes_good: good,
        bytes_unreadable: unreadable,
        bytes_pending: 0,
        recovered_this_pass: good,
        complete: !halted && unreadable == 0,
        halted,
    }
}

#[test]
fn copy_verdict_reports_a_halt_and_a_zero_recovery_as_failures() {
    // Ctrl-C after 5 GB: a partial ISO, resumable from the mapfile.
    assert_eq!(
        copy_verdict(&copy_result(true, 5_000_000_000, 0)),
        CopyVerdict::Interrupted
    );
    // Ran to the end and read NOTHING: the ISO on disk is all zeroes.
    assert_eq!(
        copy_verdict(&copy_result(false, 0, 50_000_000_000)),
        CopyVerdict::NoData
    );
    // A single recovered byte is enough to have produced something.
    assert_eq!(
        copy_verdict(&copy_result(false, 1, 0)),
        CopyVerdict::Complete
    );
    // 4 KiB of the disc never arrived. The image is usable and worth
    // keeping — it is not the disc, and the exit code has to say so.
    assert_eq!(
        copy_verdict(&copy_result(false, 25_000_000_000, 4096)),
        CopyVerdict::Lossy
    );
    // A halt that also recovered nothing reads as the halt — it is
    // resumable, and telling the user they stopped it is more useful.
    assert_eq!(
        copy_verdict(&copy_result(true, 0, 0)),
        CopyVerdict::Interrupted
    );
}

#[test]
fn a_partial_recovery_is_never_graded_as_a_clean_image() {
    // 4 KiB unreadable out of 25 GB: two sectors of the user's film, gone.
    assert_ne!(
        copy_verdict(&copy_result(false, 25_000_000_000, 4096)),
        CopyVerdict::Complete,
        "an image with unreadable sectors must not grade as a clean one"
    );
    // Sectors still PENDING are loss too — they were attempted and skipped,
    // and nothing later in a single-pass run will pick them up.
    let mut pending = copy_result(false, 25_000_000_000, 0);
    pending.bytes_pending = 8192;
    pending.complete = false; // as the engine derives it
    assert_ne!(
        copy_verdict(&pending),
        CopyVerdict::Complete,
        "pending sectors are unread bytes; the image is short of them"
    );
    // The clean sweep is untouched: this must not fail every ordinary rip.
    assert_eq!(
        copy_verdict(&copy_result(false, 25_000_000_000, 0)),
        CopyVerdict::Complete
    );
}

#[test]
fn a_lossy_copy_reports_its_loss_and_exits_with_its_own_code() {
    // NOTHING readable (or an interrupted sweep) is the only hard failure
    // (exit 1). A lossy-but-usable image is KEPT and gets its OWN distinct
    // exit code — never silently folded into 0 (clean) or 1 (no image).
    assert_eq!(disc_copy_exit_code(CopyVerdict::Complete), 0);
    assert_eq!(
        disc_copy_exit_code(CopyVerdict::Lossy),
        DISC_COPY_DAMAGED_EXIT
    );
    assert_ne!(
        DISC_COPY_DAMAGED_EXIT, 0,
        "a damaged copy must not read as clean to a scripted caller"
    );
    assert_ne!(
        DISC_COPY_DAMAGED_EXIT, 1,
        "a damaged (but kept) copy must not read as a hard failure"
    );
    for v in [CopyVerdict::NoData, CopyVerdict::Interrupted] {
        assert_eq!(
            disc_copy_exit_code(v),
            1,
            "{v:?} must exit 1 — there is no usable image to keep"
        );
    }

    // The reporting half, source-pinned (needs a real drive): the loss block
    // must be reached by the VERDICT, never `if multipass` — and must always
    // print the retry hint, or unretried pending bytes go unmentioned.
    let quiet = Output::new(false, true);
    let lossy = freemkv_engine::CopyResult {
        bytes_total: 10 << 20,
        bytes_good: 9 << 20,
        bytes_unreadable: 1 << 20,
        bytes_pending: 0,
        recovered_this_pass: 0,
        complete: false,
        halted: false,
    };
    let (code, printed) = crate::output::capture(|| {
        super::render_copy(
            &lossy,
            None,
            std::path::Path::new("/nonexistent/x.iso"),
            std::time::Instant::now(),
            &quiet,
        )
    });
    assert_eq!(code, DISC_COPY_DAMAGED_EXIT, "{printed}");
    assert!(
        printed.contains("--multipass"),
        "a damaged copy reports its loss and the retry even under -q: {printed:?}"
    );

    let src = include_str!("pipe.rs").replace("\r\n", "\n");
    let start = src
        .find("\nfn render_copy(")
        .expect("a finished copy is graded in one place");
    let end = start + src[start..].find("\n}\n").expect("render_copy ends");
    let arm = &src[start..end];
    assert!(
        !arm.contains("if multipass {"),
        "the loss report must not depend on the recovery strategy"
    );
    assert!(
        arm.contains("rip.mapfile_summary") && arm.contains("disc_copy_exit_code(verdict)"),
        "a lossy sweep must print what it lost and return its own exit code"
    );
    assert!(
        arm.contains("retry_with_multipass_hint()"),
        "a lossy sweep must always point at another run"
    );
}

/// FK6 (inverts the `1aa14e0` test): KU §2.1 invariant 5, "Memory only. No key byte and
/// no raw VID is written to any file". A whole-disc copy hands the mapfile neither.
#[test]
fn a_whole_disc_copy_persists_no_keys() {
    use crate::ku_fixtures::*;
    let fx = bd_image(&[Some(K1)], 1);
    for raw in [false, true] {
        let dir = TempDir::new("fk6");
        let src = fx.write(dir.path(), "src.iso");
        let iso = dir.path().join("disc.iso");
        let plan = super::cli_plan(
            &format!("iso://{}", src.display()),
            &format!("iso://{}", iso.display()),
            &super::KeyConfig::default(),
            (raw, false, false),
        );
        let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
        let out = Output::new(false, true);
        let code = crate::rip_keys::with_sources(f, || {
            super::whole_disc(&plan, &libfreemkv::Halt::new(), &out)
        });
        assert_eq!(code, 0, "the copy ran");
        let map = std::fs::read_to_string(freemkv_engine::mapfile_path_for(&iso)).unwrap();
        assert!(
            !map.contains("freemkv-uk") && !map.contains("freemkv-vid:"),
            "{map}"
        );
        // Neither the key nor the VID, raw or as hex, in the mapfile (or anywhere).
        assert_no_secret_on_disk(dir.path(), &[K1, VID]);
    }
}

#[test]
fn a_halted_or_holed_extraction_exits_nonzero() {
    assert!(extract_succeeded(false, true));
    assert!(
        !extract_succeeded(true, false),
        "a halted extract is a failure"
    );
    assert!(
        !extract_succeeded(false, false),
        "a holed tree is a failure"
    );
    // Contradictory input still fails closed rather than reporting success.
    assert!(!extract_succeeded(true, true));
}

#[test]
fn a_single_title_rip_never_downgrades_a_failure_to_a_skip() {
    use freemkv_engine::{TitleAction, TitleResult, decide_title};

    let (multi, explicit) = title_policy(1, &[2], false);
    assert!(!multi, "one job is not a multi-title rip");
    assert!(explicit, "a named -t is an explicit selection");
    assert!(matches!(
        decide_title(&TitleResult::Failed, false, multi, explicit),
        TitleAction::StopFatal
    ));

    // A real all-titles batch: an incidental uncrackable stub is skippable.
    let (multi, explicit) = title_policy(12, &[], true);
    assert!(multi);
    assert!(
        !explicit,
        "-t all asks for everything, which is not the same as naming titles"
    );

    // `-t all` expanded into a list must still read as non-explicit, or the
    // first menu stub on an obfuscated disc aborts the whole rip.
    let (_, explicit) = title_policy(12, &[1, 2, 3], true);
    assert!(!explicit);
    // Named titles without -t all stay explicit even in a batch.
    let (multi, explicit) = title_policy(3, &[1, 2, 3], false);
    assert!(multi && explicit);
    // No jobs, no flags: neither.
    assert_eq!(title_policy(0, &[], false), (false, false));
}

fn audio(pid: u16, lang: &str) -> libfreemkv::Stream {
    libfreemkv::Stream::Audio(libfreemkv::AudioStream {
        pid,
        codec: libfreemkv::Codec::TrueHd,
        channels: libfreemkv::AudioChannels::Stereo,
        language: lang.into(),
        sample_rate: libfreemkv::SampleRate::S48,
        secondary: false,
        purpose: libfreemkv::LabelPurpose::Normal,
        label: String::new(),
    })
}

#[test]
fn a_requested_language_absent_from_the_title_is_an_error_for_a_single_title() {
    let mut title = libfreemkv::DiscTitle::empty();
    title.streams = vec![audio(0x1100, "eng")];
    let streams = freemkv_engine::StreamChoice {
        audio: freemkv_engine::StreamFilter::Langs(vec!["jpn".into()]),
        subtitles: freemkv_engine::StreamFilter::All.into(),
    };

    let err = check_selection_coverage(&streams, &title, 1, false, &quiet())
        .expect_err("a single-title rip must fail rather than ship a soundless file");
    assert!(
        err.to_lowercase().contains("eng") || err.contains("jpn"),
        "the message should name the languages involved: {err}"
    );
    // The title loop renders the failure with the level word; it must not be there already.
    let level = crate::strings::get(crate::messaging::Level::Error.locale_key());
    assert_eq!(
        super::render_error(&err).matches(&level).count(),
        1,
        "{err}"
    );

    // Same title in a batch: warn loudly and keep going. Assert the
    // warning is actually emitted, not just that the call returns Ok, or
    // a regression that silently drops the warning would still pass.
    let (batch_res, printed) =
        crate::output::capture(|| check_selection_coverage(&streams, &title, 4, true, &quiet()));
    assert!(
        batch_res.is_ok(),
        "a batch must not hard-fail on one title of the wrong language"
    );
    assert!(
        printed.contains("jpn") || printed.to_lowercase().contains("eng"),
        "a batch must still warn loudly about the unmatched language: {printed:?}"
    );

    // A language the title DOES carry is not an error in either mode.
    let matched = freemkv_engine::StreamChoice {
        audio: freemkv_engine::StreamFilter::Langs(vec!["eng".into()]),
        subtitles: freemkv_engine::StreamFilter::All.into(),
    };
    assert!(check_selection_coverage(&matched, &title, 1, false, &quiet()).is_ok());
}

/// A `PipeFail` classification is what the loop acts on, so the
/// constructors must not collapse into each other.
#[test]
fn the_failure_classes_stay_distinct() {
    assert_eq!(
        PipeFail::fatal("x".into()).result,
        freemkv_engine::TitleResult::Failed
    );
    assert_eq!(
        PipeFail::halted("x".into()).result,
        freemkv_engine::TitleResult::Halted
    );
}
