//! `ui-review`: the CI side of the desktop apps' screenshot review.
//!
//! The GUI gate (`tests/gui_gate.py`, `src/gui_gate.rs`) captures every screen of the Linux and
//! Windows apps, per language and scale, into the `gui-gate-<platform>` artifacts. This tool
//! lays one run's captures out as `<platform>/<lang>-<scale>/<name>.png` in a checkout of the
//! `ui-screenshots` branch and writes the gallery, `index.md`, that links them for sign-off.
//!
//! ```text
//! ui-review publish --shots DIR --site DIR --repo OWNER/NAME --branch REF --sha SHA
//!                   --date DATE --run URL [--summary FILE]
//! ```
//!
//! `--shots` holds the downloaded artifacts (`gui-gate-linux/en-100/*.png`, …); `--site` is
//! the branch's work tree, whose run is replaced. `--summary` gets a Markdown summary (CI
//! passes `$GITHUB_STEP_SUMMARY`).

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
    let (command, flags) = args.split_first().ok_or(
        "usage: ui-review publish --shots DIR --site DIR --repo R --sha S --date D --run URL",
    )?;
    let flags = parse_flags(flags)?;
    let get = |k: &str| {
        flags
            .get(k)
            .cloned()
            .ok_or_else(|| format!("--{k} is required"))
    };
    match command.as_str() {
        "publish" => {
            let info = RunInfo {
                repo: get("repo")?,
                branch: get("branch")?,
                sha: get("sha")?,
                date: get("date")?,
                run: get("run")?,
            };
            let site = PathBuf::from(get("site")?);
            let summary = gallery::publish(&PathBuf::from(get("shots")?), &site, &info)
                .map_err(|e| e.to_string())?;
            print!("{summary}");
            if let Some(path) = flags.get("summary") {
                append(path, &summary).map_err(|e| format!("{path}: {e}"))?;
            }
            Ok(())
        }
        other => Err(format!("unknown command {other:?}")),
    }
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
