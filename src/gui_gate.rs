//! qa's GUI gate: what the Windows and GTK shells check about themselves when a debug build
//! runs with `FMKV_GATE=<dir>` over the synthetic disc folder `tests/gui_fixture.py` writes.
//!
//! The shells measure their own widgets (a cell's text against its column, a path against its
//! field) and capture themselves; the arithmetic and the verdict live here, toolkit-free, so they
//! are tested on every host. Not `cfg`-gated for the same reason as `win_layout`.
//!
//! The review tour (the `ui-screenshots` gallery) also puts the model into states the fixture
//! never reaches by itself: fake drives on the ready screen, a rip in progress, each result and
//! the review banner. Those states are built here, from the model's own types, so both shells
//! show the same thing.

use crate::engine::{RunOutcome, RunState, TitlePreparation};
use crate::ui::{App, HomeDrive, InfoRows, LogKind, Page, TreeReview};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The default destination the gate puts in Settings: on Windows long enough that the old
/// 280 px field cut it off; on Linux as long as a Settings row holds at 150% type.
pub const LONG_DEST: &str = if cfg!(windows) {
    r"C:\Users\Public\Videos\freemkv rips\Archive of my own discs"
} else {
    "/mnt/library/Videos/freemkv/Archive 2026"
};

/// The output folder the disc page shows during the gate, so its free-space line reads the
/// same on every run and the screenshot diff does not flag it: Windows has no such drive
/// ("—"), and on Linux the nearest existing folder is `/proc`, which never has any free space.
pub const REVIEW_DEST: &str = if cfg!(windows) {
    r"R:\Videos\freemkv"
} else {
    "/proc/freemkv/Videos"
};

/// A rectangle of a captured image, in top-down pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Whether `r` of a 32-bit image `width` × `height` holds more than one colour: text or a rule
/// on its background, not a blank fill. `bottom_up` is a Windows DIB's row order. A region
/// that falls outside the image has no ink.
#[must_use]
pub fn has_ink(px: &[u8], width: i32, height: i32, bottom_up: bool, r: Region) -> bool {
    let (x0, y0) = (r.x.max(0), r.y.max(0));
    let (x1, y1) = ((r.x + r.w).min(width), (r.y + r.h).min(height));
    let mut first: Option<&[u8]> = None;
    for y in y0..y1 {
        let row = if bottom_up { height - 1 - y } else { y };
        for x in x0..x1 {
            let i = (row as usize * width as usize + x as usize) * 4;
            let Some(p) = px.get(i..i + 3) else {
                return false;
            };
            match first {
                None => first = Some(p),
                Some(f) if f != p => return true,
                Some(_) => {}
            }
        }
    }
    false
}

/// The gate's findings, in order.
#[derive(Debug, Default)]
pub struct Gate {
    checks: Vec<(bool, String)>,
    notes: Vec<String>,
}

impl Gate {
    pub fn check(&mut self, name: &str, ok: bool, detail: impl AsRef<str>) {
        self.checks
            .push((ok, format!("{name} — {}", detail.as_ref())));
    }

    /// A line for the report that is not a check: a review capture this run could not take,
    /// and why. The screen is then missing from the gallery, which flags it there.
    pub fn note(&mut self, name: &str, detail: impl AsRef<str>) {
        self.notes.push(format!("{name} — {}", detail.as_ref()));
    }

    /// Whether every check passed; a gate that checked nothing has not passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|(ok, _)| *ok)
    }

    /// One `GATE PASS …` / `GATE FAIL …` line per check, a `GATE NOTE …` line per note, then
    /// the verdict.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out: String = self
            .checks
            .iter()
            .map(|(ok, line)| format!("GATE {} {line}\n", if *ok { "PASS" } else { "FAIL" }))
            .collect();
        for line in &self.notes {
            out.push_str(&format!("GATE NOTE {line}\n"));
        }
        let failed = self.checks.iter().filter(|(ok, _)| !ok).count();
        out.push_str(&if self.passed() {
            format!("GATE OK: {} checks\n", self.checks.len())
        } else {
            format!("GATE FAILED: {failed} of {} checks\n", self.checks.len())
        });
        out
    }
}

// ── the review tour's model states ──────────────────────────────────────────

/// The ready screen's drives: one holding a disc, one empty, one whose status could not be
/// read. Named the way each platform names a drive.
#[must_use]
pub fn fake_drives() -> Vec<HomeDrive> {
    let drives = [
        (
            "HL-DT-ST BD-RE BU40N (rev 1.04)",
            "HL-DT-ST BD-RE BU40N",
            Some(true),
        ),
        ("ASUS BW-16D1HT (rev 3.10)", "ASUS BW-16D1HT", Some(false)),
        (
            "PIONEER BD-RW BDR-XD08 (rev 1.02)",
            "PIONEER BD-RW BDR-XD08",
            None,
        ),
    ];
    drives
        .into_iter()
        .enumerate()
        .map(|(i, (model, legacy, present))| {
            let (device, name) = if cfg!(windows) {
                let letter = char::from(b'D' + i as u8);
                (format!(r"\\.\{letter}:"), format!("{letter}:"))
            } else {
                (format!("/dev/sg{i}"), format!("/dev/sr{i}"))
            };
            HomeDrive {
                device,
                display: format!("{name}  {model}"),
                legacy_label: legacy.into(),
                present,
            }
        })
        .collect()
}

/// The rip states the progress page is captured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rip {
    /// One title: the top bar only.
    Single,
    /// Title 2 of 3 being written: both bars.
    Multi,
    /// Title 1 of 3 being read into the staging image first.
    Reading,
    /// Cancel pressed: the run winding down.
    Stopping,
}

/// Put `app` on the progress page of a rip that is not happening: a run state the shells
/// read like a real one, with fixed numbers so the captures compare between runs.
pub fn show_rip(app: &mut App, rip: Rip) {
    let multi = rip != Rip::Single;
    let state = RunState::default();
    {
        let mut p = state.prog.lock().unwrap_or_else(|e| e.into_inner());
        p.bytes_done = 4_312_000_000;
        p.bytes_total = 9_870_000_000;
        p.speed_bps = 41_400_000;
        p.eta_secs = Some(754);
        p.title_pct = 42.0;
        p.title_started = None;
        p.batch_pct = multi.then_some(57.0);
        p.position = match rip {
            Rip::Single => None,
            Rip::Reading => Some((1, 3)),
            Rip::Multi | Rip::Stopping => Some((2, 3)),
        };
        p.preparation = if rip == Rip::Reading {
            TitlePreparation::Reading
        } else {
            TitlePreparation::NotNeeded
        };
    }
    state
        .titles_done
        .store(usize::from(multi), Ordering::Relaxed);
    state.cancel.store(rip == Rip::Stopping, Ordering::Relaxed);
    let (source, output) = if cfg!(windows) {
        (
            r"D:\media\iso\Movie.iso",
            r"C:\Users\me\Videos\Movie_t1.mkv",
        )
    } else {
        ("/media/iso/Movie.iso", "/home/me/Videos/Movie_t1.mkv")
    };
    app.info = Some(InfoRows {
        source: source.into(),
        source_file: "Movie.iso".into(),
        source_size: crate::ui::fmt_bytes(9_870_000_000),
        read_rate: crate::ui::rate_text(41_400_000, true),
        output_file: output.into(),
        output_size: crate::ui::fmt_bytes(4_312_000_000),
        free_space: crate::ui::fmt_bytes(243_000_000_000),
    });
    app.run = Some(Arc::new(state));
    app.run_titles = if multi { 3 } else { 1 };
    app.run_started = None;
    app.page = Page::Progress;
}

/// Undo [`show_rip`]: no run, no Information rows, back on `page`.
pub fn end_rip(app: &mut App, page: Page) {
    app.run = None;
    app.run_titles = 0;
    app.info = None;
    app.page = page;
}

/// Put `app` on the result page of a run that ended with `outcome`. The summaries are the
/// kind of English line the engine writes there.
pub fn show_result(app: &mut App, outcome: RunOutcome) {
    app.result_summary = match outcome {
        RunOutcome::Completed => "2 title(s) written",
        RunOutcome::Cancelled => "Cancelled after 1 of 2 titles; the finished title was kept",
        RunOutcome::Failed => "No key for this disc: title 1 could not be decrypted",
    }
    .into();
    app.result_outcome = outcome;
    app.page = Page::Result;
}

/// A title choice the disc does not prove, so the disc page shows its review banner.
#[must_use]
pub fn review() -> TreeReview {
    TreeReview {
        reason: freemkv_engine::SelectionReviewReason::MissingEpisodeRoster,
        suggestion: None,
    }
}

/// The notices a session can log (red in the GTK log, `!` in the Windows one): an
/// unreadable stretch, a dropped file the app cannot open and a failed drive scan.
pub fn notices(app: &mut App) {
    let dropped = if cfg!(windows) {
        r"C:\Users\me\Downloads\notes.txt"
    } else {
        "/home/me/Downloads/notes.txt"
    };
    app.say(
        LogKind::Notice,
        &crate::strings::fmt("gui.log.unreadable", &[("n", "12")]),
    );
    app.say(
        LogKind::Notice,
        &crate::strings::fmt("gui.log.not_supported", &[("p", dropped)]),
    );
    app.say(
        LogKind::Notice,
        &crate::strings::get("gui.log.drive_scan_failed"),
    );
}

#[cfg(test)]
#[path = "gui_gate_tests.rs"]
mod tests;
