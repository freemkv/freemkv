//! Lay a run's captures out on the `ui-screenshots` branch and write its gallery.

use crate::RunInfo;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io;
use std::path::Path;

/// What the gate writes beside its captures; published so a missing screen's reason (a
/// `GATE NOTE`) is one click from the gallery.
const REPORTS: [&str; 2] = ["gate.txt", "tree-gate.txt"];

/// One run's captures: platform → screen name → the variants (`en-100`, …) it was taken in.
pub type Screens = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;

/// A capture's or variant's name is lower-case letters, digits and dashes: it becomes a path
/// and a Markdown link, so nothing else gets through.
pub fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Replace the run in `site` with the artifacts in `shots`, write `index.md` and `run.txt`,
/// and return the job summary.
pub fn publish(shots: &Path, site: &Path, info: &RunInfo) -> io::Result<String> {
    clear_run(site)?;
    let screens = copy_run(shots, site)?;
    std::fs::write(site.join("run.txt"), run_txt(info))?;
    std::fs::write(site.join("index.md"), index(&screens, info))?;
    Ok(summary(&screens, info))
}

/// Everything but the git metadata goes: the branch holds one run.
fn clear_run(site: &Path) -> io::Result<()> {
    std::fs::create_dir_all(site)?;
    for entry in std::fs::read_dir(site)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Copy `gui-gate-<platform>/<variant>/<name>.png` (and the gate reports) to
/// `<platform>/<variant>/`, returning what was copied.
fn copy_run(shots: &Path, site: &Path) -> io::Result<Screens> {
    let mut screens = Screens::new();
    for artifact in sorted_dirs(shots)? {
        let Some(platform) = file_name(&artifact)
            .strip_prefix("gui-gate-")
            .map(str::to_string)
            .filter(|p| safe_name(p))
        else {
            continue;
        };
        for variant_dir in sorted_dirs(&artifact)? {
            let variant = file_name(&variant_dir);
            if !safe_name(&variant) {
                continue;
            }
            let out = site.join(&platform).join(&variant);
            for entry in std::fs::read_dir(&variant_dir)? {
                let path = entry?.path();
                let file = file_name(&path);
                if REPORTS.contains(&file.as_str()) {
                    std::fs::create_dir_all(&out)?;
                    std::fs::copy(&path, out.join(&file))?;
                    continue;
                }
                let Some(name) = file.strip_suffix(".png").filter(|n| safe_name(n)) else {
                    continue;
                };
                std::fs::create_dir_all(&out)?;
                std::fs::copy(&path, out.join(&file))?;
                screens
                    .entry(platform.clone())
                    .or_default()
                    .entry(name.to_string())
                    .or_default()
                    .insert(variant.clone());
            }
        }
    }
    Ok(screens)
}

fn sorted_dirs(dir: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// English first, then German and the Arabic (right-to-left) run; each at 100% before larger.
pub fn variant_order(v: &str) -> (usize, u32, String) {
    let (lang, scale) = v.split_once('-').unwrap_or((v, ""));
    let rank = ["en", "de", "ar"]
        .iter()
        .position(|l| *l == lang)
        .unwrap_or(usize::MAX);
    (rank, scale.parse().unwrap_or(u32::MAX), v.to_string())
}

/// Every variant any screen of `platform` was taken in, in display order.
pub fn variants(by_name: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    let mut all: Vec<String> = by_name
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    all.sort_by_key(|v| variant_order(v));
    all
}

pub fn platform_title(p: &str) -> String {
    match p {
        "linux" => "Linux".into(),
        "windows" => "Windows".into(),
        "macos" => "macOS".into(),
        other => other.into(),
    }
}

/// `run.txt`: the run the branch holds, `key=value` per line.
pub fn run_txt(info: &RunInfo) -> String {
    format!(
        "repo={}\nbranch={}\nsha={}\ndate={}\nrun={}\n",
        info.repo, info.branch, info.sha, info.date, info.run
    )
}

/// The heading line every page and summary opens with.
pub fn header(info: &RunInfo) -> String {
    let short: String = info.sha.chars().take(12).collect();
    format!(
        "`{}` at [`{short}`](https://github.com/{}/commit/{}) · {} · [CI run]({})\n",
        info.branch, info.repo, info.sha, info.date, info.run
    )
}

/// A thumbnail linking to the full-size capture at `path` (relative to the branch root).
pub fn thumb(path: &str) -> String {
    format!("<a href=\"{path}\"><img src=\"{path}\" width=\"220\"></a>")
}

/// Every screen of every platform: one table per screen, one column per variant.
pub fn all_screens(out: &mut String, screens: &Screens) {
    for (platform, by_name) in screens {
        let variants = variants(by_name);
        let _ = writeln!(out, "## {}\n", platform_title(platform));
        let _ = writeln!(
            out,
            "{} screens · gate reports: {}\n",
            by_name.len(),
            variants
                .iter()
                .map(|v| format!("[{v}]({platform}/{v}/gate.txt)"))
                .collect::<Vec<_>>()
                .join(" · ")
        );
        for (name, have) in by_name {
            let _ = writeln!(out, "### {name}\n");
            let _ = writeln!(out, "| {} |", variants.join(" | "));
            let _ = writeln!(out, "|{}", "---|".repeat(variants.len()));
            let cells: Vec<String> = variants
                .iter()
                .map(|v| {
                    if have.contains(v) {
                        thumb(&format!("{platform}/{v}/{name}.png"))
                    } else {
                        "—".into()
                    }
                })
                .collect();
            let _ = writeln!(out, "| {} |\n", cells.join(" | "));
        }
    }
}

fn index(screens: &Screens, info: &RunInfo) -> String {
    let mut out = String::from("# freemkv desktop UI screenshots\n\n");
    out.push_str(&header(info));
    out.push('\n');
    all_screens(&mut out, screens);
    out
}

fn summary(screens: &Screens, info: &RunInfo) -> String {
    let mut out = String::from("## UI screenshots\n\n");
    out.push_str(&header(info));
    out.push('\n');
    for (platform, by_name) in screens {
        let captures: usize = by_name.values().map(BTreeSet::len).sum();
        let _ = writeln!(
            out,
            "- {}: {} screens, {captures} captures",
            platform_title(platform),
            by_name.len()
        );
    }
    let _ = writeln!(
        out,
        "\nGallery: https://github.com/{}/blob/ui-screenshots/index.md",
        info.repo
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_names_are_published() {
        assert!(safe_name("disc-page-titles-open"));
        assert!(safe_name("en-100"));
        assert!(!safe_name(""));
        assert!(!safe_name("../x"));
        assert!(!safe_name("a b"));
        assert!(!safe_name("Menu"));
    }

    #[test]
    fn variants_read_english_first_and_small_before_large() {
        let mut v = vec!["ar-100", "de-200", "en-200", "de-100", "en-100", "xx-100"];
        v.sort_by_key(|v| variant_order(v));
        assert_eq!(
            v,
            ["en-100", "en-200", "de-100", "de-200", "ar-100", "xx-100"]
        );
    }

    #[test]
    fn a_missing_variant_shows_a_dash() {
        let mut screens = Screens::new();
        let linux = screens.entry("linux".into()).or_default();
        linux
            .entry("about".into())
            .or_default()
            .insert("en-100".into());
        linux
            .entry("menu".into())
            .or_default()
            .insert("de-100".into());
        let mut out = String::new();
        all_screens(&mut out, &screens);
        assert!(out.contains("## Linux"), "{out}");
        assert!(out.contains("| en-100 | de-100 |"), "{out}");
        assert!(out.contains("linux/en-100/about.png"), "{out}");
        assert!(out.contains("| — |"), "{out}");
    }
}
