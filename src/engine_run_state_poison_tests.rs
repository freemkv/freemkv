// A panicking worker must not turn its verdict into "Completed" — this pins
// RunState::outcome_now's poison recovery, not just what the accessor happens to do today.
#[test]
fn a_poisoned_lock_does_not_turn_a_failure_into_a_completion() {
    use super::{RunOutcome, RunState};
    use std::sync::Arc;

    let st = Arc::new(RunState::default());
    *st.outcome.lock().unwrap() = RunOutcome::Failed;
    *st.summary.lock().unwrap() = "aborted: loss exceeded tolerance".to_string();

    // Poison both locks the way a panicking worker would.
    let st2 = Arc::clone(&st);
    let _ = std::thread::spawn(move || {
        let _g1 = st2.outcome.lock().unwrap();
        let _g2 = st2.summary.lock().unwrap();
        panic!("worker died holding the verdict");
    })
    .join();
    assert!(
        st.outcome.is_poisoned() && st.summary.is_poisoned(),
        "fixture invalid: the locks must actually be poisoned"
    );

    assert_eq!(
        st.outcome_now(),
        RunOutcome::Failed,
        "a poisoned lock rendered a FAILED run as Completed — the heading \
             then says Finished over a rip that did not produce its deliverable"
    );
    assert_eq!(
        st.summary_now(),
        "aborted: loss exceeded tolerance",
        "the summary the worker had already written was discarded"
    );
}

// The GUI core's only Sink must still deliver lines/progress after a worker panics — UiSink
// recovers the poisoned lock rather than silently dropping data.
#[test]
fn the_ui_sink_still_delivers_lines_and_progress_through_a_poisoned_lock() {
    use super::{Prog, RunState, UiSink};
    use freemkv_engine as fe;
    use freemkv_engine::Sink as _;
    use std::sync::Arc;

    let st = Arc::new(RunState::default());
    // Poison both buffers the way a panicking worker would.
    let st2 = Arc::clone(&st);
    let _ = std::thread::spawn(move || {
        // Bound to a differently-named local on purpose: this fixture MUST
        // unwrap (poisoning is the point), and the source-scraping pin
        // below would otherwise match its own sibling fixture code.
        let buf = &st2.lines;
        let _g1 = buf.lock().unwrap();
        let _g2 = st2.prog.lock().unwrap();
        panic!("worker died holding the log buffer");
    })
    .join();
    assert!(
        st.lines.is_poisoned() && st.prog.is_poisoned(),
        "fixture invalid: both locks must actually be poisoned"
    );

    let sink = UiSink(Arc::clone(&st));
    sink.log(fe::Level::Warn, "E7022: no key for this disc");
    sink.progress(&fe::Progress {
        bytes_done: 7,
        bytes_total: 11,
        speed_bps: 3,
        eta_secs: Some(5),
        sectors_bad: 1,
        ..Default::default()
    });

    assert_eq!(
        *st.lines.lock().unwrap_or_else(|e| e.into_inner()),
        vec!["E7022: no key for this disc".to_string()],
        "the sink dropped the line that explains the failure — after a \
             panic the log is the only record there is"
    );
    let Prog {
        bytes_done,
        bytes_total,
        ..
    } = *st.prog.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        (bytes_done, bytes_total),
        (7, 11),
        "the sink stopped publishing progress, so the bar freezes with no \
             explanation for the rest of the run"
    );
}

// The staged mux reports its titles only as engine events; they must move the bars.
#[test]
fn engine_title_events_restart_the_top_bar_and_advance_the_bottom_one() {
    use super::{RunState, UiSink};
    use freemkv_engine as fe;
    use freemkv_engine::Sink as _;
    use std::sync::Arc;

    let st = Arc::new(RunState::default());
    st.plan_titles(vec![(0, 100), (1, 100)]);
    let sink = UiSink(Arc::clone(&st));
    let failed = std::io::Error::other("title failed");
    sink.event(&fe::Event::TitleStart {
        idx: 0,
        dest: "mkv://a",
    });
    sink.progress(&fe::Progress {
        bytes_done: 40,
        bytes_total: 100,
        ..Default::default()
    });
    let p = *st.prog.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!((p.title_pct, p.batch_pct), (40.0, Some(20.0)));
    sink.event(&fe::Event::TitleDone {
        idx: 0,
        dest: "mkv://a",
        result: Err(&failed),
    });
    sink.event(&fe::Event::TitleStart {
        idx: 1,
        dest: "mkv://b",
    });
    let p = *st.prog.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!((p.title_pct, p.batch_pct), (0.0, Some(50.0)));
}

// Makes outcome_now's doc claim true instead of merely written down: every `lines` lock in
// engine.rs/main.rs must recover from poison, verified by source scan.
#[test]
fn every_lines_lock_recovers_from_poison() {
    let needle = concat!("lines", ".lock()");
    // Matches the recovery pattern, not one exact literal, since both the
    // closure form and the `PoisonError::into_inner` form appear in the
    // tree — a pin that only knew one would fail on correct code.
    let recovering = concat!(".unwrap", "_or_else(");
    let mut found = 0usize;
    for (name, raw) in [
        ("engine.rs", include_str!("engine.rs")),
        ("main.rs", include_str!("main.rs")),
    ] {
        // Whitespace-collapsed so a lock split over four lines by rustfmt
        // reads the same as one written inline — the formatter must not be
        // able to hide a regression from this pin.
        let src: String = raw.split_whitespace().collect();
        let mut at = 0usize;
        while let Some(i) = src[at..].find(needle) {
            let pos = at + i;
            let tail = &src[pos + needle.len()..];
            let head = &tail[..tail.len().min(80)];
            assert!(
                tail.starts_with(recovering) && head.contains("into_inner"),
                "{name}: a log-buffer lock does not recover from poison \
                     (site {}); a worker that panicked mid-run poisoned it, and \
                     this turns one dead thread into a second panic — or a \
                     silently discarded line — losing the diagnostic that \
                     explains the first. Near: {}",
                found + 1,
                &tail[..tail.len().min(60)]
            );
            found += 1;
            at = pos + needle.len();
        }
    }
    assert!(
        found >= 8,
        "expected to inspect every log-buffer lock in engine.rs and \
             main.rs, found only {found} — the needle stopped matching and \
             this pin is now vacuous"
    );
}
