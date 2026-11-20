//! `ui-review`: the CI side of the desktop apps' screenshot review.
//!
//! The GUI gate (`tests/gui_gate.py`, `src/gui_gate.rs`) captures every screen of the Linux and
//! Windows apps, per language and scale, into the `gui-gate-<platform>` artifacts. This tool
//! lays one run's captures out as `<platform>/<lang>-<scale>/<name>.png` in a checkout of the
//! `ui-screenshots` branch, compares each with the approved one under `baseline/` (`diff`),
//! and writes the gallery, `index.md`: the changed screens first, then everything.
//!
//! ```text
//! ui-review publish --shots DIR --site DIR --repo OWNER/NAME --branch REF --sha SHA
//!                   --date DATE --run URL [--summary FILE] [TOLERANCES]
//! ui-review approve --site DIR [--sha SHA] [--summary FILE] [TOLERANCES]
//! TOLERANCES: [--threshold 0.1] [--min-pixels 24]
//! ```
//!
//! `publish`: `--shots` holds the downloaded artifacts (`gui-gate-linux/en-100/*.png`, …);
//! `--site` is the branch's work tree, whose run is replaced and whose baseline is kept.
//! `approve`: the run on the branch becomes the baseline; with `--sha`, only if it is that
//! commit's run. `--summary` gets a Markdown summary (CI passes `$GITHUB_STEP_SUMMARY`).
//! A changed screen is reported, never an error: the exit code is for broken input only.

mod diff;
mod gallery;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

/// What the run was: shown at the top of the gallery and kept in `run.txt`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunInfo {
    pub repo: String,
    pub branch: String,
    pub sha: String,
    pub date: String,
    pub run: String,
}

const USAGE: &str = "usage: ui-review publish --shots DIR --site DIR --repo R --branch B --sha S \
                     --date D --run URL | ui-review approve --site DIR [--sha S]";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ui-review: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let (command, flags) = args.split_first().ok_or(USAGE)?;
    let flags = parse_flags(flags)?;
    let get = |k: &str| {
        flags
            .get(k)
            .cloned()
            .ok_or_else(|| format!("--{k} is required"))
    };
    let opt = tolerances(&flags)?;
    let site = PathBuf::from(get("site")?);
    let summary = match command.as_str() {
        "publish" => {
            let info = RunInfo {
                repo: get("repo")?,
                branch: get("branch")?,
                sha: get("sha")?,
                date: get("date")?,
                run: get("run")?,
            };
            let shots = PathBuf::from(get("shots")?);
            let (summary, report) =
                gallery::publish(&shots, &site, &info, opt).map_err(|e| e.to_string())?;
            // An annotation on the run: seen without opening the summary, and not a failure.
            if !report.changed.is_empty() {
                println!(
                    "::warning title=UI screenshots changed::{} captures differ from the approved \
                     baseline; review them in the ui-screenshots gallery",
                    report.changed.len()
                );
            }
            summary
        }
        "approve" => gallery::approve(&site, flags.get("sha").map(String::as_str), opt)
            .map_err(|e| e.to_string())?,
        other => return Err(format!("unknown command {other:?}; {USAGE}")),
    };
    print!("{summary}");
    if let Some(path) = flags.get("summary") {
        append(path, &summary).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

/// The diff's tolerances, the defaults unless given.
fn tolerances(flags: &HashMap<String, String>) -> Result<diff::Options, String> {
    let mut opt = diff::Options::default();
    if let Some(t) = flags.get("threshold") {
        opt.threshold = t
            .parse()
            .ok()
            .filter(|t| (0.0..=1.0).contains(t))
            .ok_or_else(|| format!("--threshold {t:?}: a number from 0 to 1"))?;
    }
    if let Some(n) = flags.get("min-pixels") {
        opt.min_pixels = n
            .parse()
            .map_err(|_| format!("--min-pixels {n:?}: a whole number"))?;
    }
    Ok(opt)
}

/// `--key value` pairs.
fn parse_flags(args: &[String]) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let key = k
            .strip_prefix("--")
            .ok_or_else(|| format!("expected --flag, got {k:?}"))?;
        let value = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
        out.insert(key.to_string(), value.clone());
    }
    Ok(out)
}

fn append(path: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}
