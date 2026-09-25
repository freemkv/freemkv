//! The shell contract: every `Cmd` produces the same `Effect` set no matter
//! which platform shell is driving it, because the core does the dispatch.
//! When the Linux GTK4 shell lands next to `mac.rs` and `windows.rs`, this
//! test is what keeps all three from drifting: a bug fix or new command
//! passes through the shared `App::dispatch` and every shell sees the same
//! effect stream, or a shell's handler is wrong and any user-visible
//! divergence is a shell bug, not a semantics bug.
//!
//! This SKELETON only covers `App::new()` — the fresh, no-source state —
//! and pins one property per `Cmd`: the KINDS of effects it emits. It is
//! deliberately not a full state-machine walk (that would need synthetic
//! scans à la `gui_model.rs`); it exists so a new `Cmd` variant added
//! without a corresponding shell handler fails an integration test
//! instead of shipping a menu item that does nothing on Linux.
//!
//! Adding a new `Cmd` variant → this test won't compile (the `match` is
//! exhaustive by design). Adding a new `Effect` variant → this test still
//! passes; a per-`Cmd` line changes only when its emitted set changes.

use freemkv::ui::{App, Cmd, Effect};

/// Structural label for an `Effect`, ignoring payload — the shell contract
/// pins WHICH effects fire, not the exact strings inside them (which are
/// covered by `strings.rs` locale tests and the model tests in `gui_model.rs`).
fn kind(e: &Effect) -> &'static str {
    match e {
        Effect::PickSource => "PickSource",
        Effect::PickOutputDir => "PickOutputDir",
        Effect::Reveal(_) => "Reveal",
        Effect::OpenUrl(_) => "OpenUrl",
        Effect::ShowSettings => "ShowSettings",
        Effect::ShowAbout => "ShowAbout",
        Effect::Redraw => "Redraw",
        Effect::StartTicking => "StartTicking",
        Effect::StopTicking => "StopTicking",
        Effect::NotifyRipFinished { .. } => "NotifyRipFinished",
        Effect::Quit => "Quit",
    }
}

fn kinds(app: &mut App, cmd: Cmd) -> Vec<&'static str> {
    app.dispatch(cmd).iter().map(kind).collect()
}

/// The default-state answer for every `Cmd`. If you add a `Cmd`, the match
/// below breaks and you must decide what its default-state effect set is.
/// Changing an existing line is a deliberate contract change — check that
/// every shell (mac.rs, windows.rs, linux.rs) still does the right thing.
fn expected_default_kinds(cmd: Cmd) -> Vec<&'static str> {
    match cmd {
        Cmd::Open => vec!["PickSource"],
        Cmd::SetOutput => vec!["PickOutputDir"],
        Cmd::Settings => vec!["ShowSettings"],
        Cmd::About => vec!["ShowAbout"],
        Cmd::Docs => vec!["OpenUrl"],
        // CheckUpdates runs the check itself and logs the outcome — the shell
        // just re-renders. It does not open a browser (that would surprise a
        // user who clicked "check for updates" and expected an in-app answer).
        Cmd::CheckUpdates => vec!["Redraw"],
        Cmd::Quit => vec!["Quit"],
        // The rest all do state-changing work that a fresh, source-less
        // `App` collapses to a single redraw — no source to close, no tree
        // to select, no run to cancel, no disc to eject.
        Cmd::Close
        | Cmd::SetFormat(_)
        | Cmd::Run
        | Cmd::Cancel
        | Cmd::Eject
        | Cmd::SelectAll
        | Cmd::SelectNone
        | Cmd::Invert
        | Cmd::ClearLog
        | Cmd::ToggleLog => vec!["Redraw"],
    }
}

#[test]
fn every_cmd_from_a_fresh_app_emits_the_same_effect_set_for_every_shell() {
    // The universe of `Cmd`s a shell menu bar can fire. `SetFormat` carries a
    // borrowed string; the exact string does not matter for effect kinds, but
    // it must be one `output_formats` produces or the model rejects it.
    let all: &[Cmd] = &[
        Cmd::Open,
        Cmd::Close,
        Cmd::SetOutput,
        Cmd::Run,
        Cmd::Cancel,
        Cmd::Eject,
        Cmd::SelectAll,
        Cmd::SelectNone,
        Cmd::Invert,
        Cmd::ClearLog,
        Cmd::ToggleLog,
        Cmd::Settings,
        Cmd::About,
        Cmd::Docs,
        Cmd::CheckUpdates,
        // Skip `Cmd::Quit` in the loop — it emits `Effect::Quit` which the
        // shell would honour by tearing down the app; still asserted below.
    ];

    for &cmd in all {
        let mut app = App::new();
        assert_eq!(
            kinds(&mut app, cmd),
            expected_default_kinds(cmd),
            "Cmd::{cmd:?} produced an unexpected effect set from a fresh App — \
             either a shell change dropped a handler or the contract needs \
             updating (do all three shells still do the right thing?)",
        );
    }

    // Format setting: needs a valid format string; the first offered
    // canonical wins so the test survives translation churn but still
    // exercises the SetFormat path.
    let mut app = App::new();
    let fmt = freemkv::ui::output_formats(false, false)
        .into_iter()
        .flatten()
        .next()
        .expect("at least one output format must be offered");
    assert_eq!(
        kinds(&mut app, Cmd::SetFormat(fmt)),
        expected_default_kinds(Cmd::SetFormat(fmt)),
    );

    // Quit stands alone: shell reads `Effect::Quit` and terminates.
    let mut app = App::new();
    assert_eq!(kinds(&mut app, Cmd::Quit), expected_default_kinds(Cmd::Quit));
}
