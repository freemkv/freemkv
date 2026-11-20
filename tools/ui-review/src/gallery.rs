//! Lay a run's captures out on the `ui-screenshots` branch, compare them with the approved
//! baseline and write the gallery.
//!
//! The branch holds the latest run as `<platform>/<lang>-<scale>/<name>.png`, the approved set
//! under `baseline/` in the same layout, a highlight per changed capture under `diff/`,
//! `run.txt` (which run this is) and `index.md`, the gallery.

use crate::RunInfo;
use crate::diff::{self, Options, Outcome};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

pub const BASELINE: &str = "baseline";
pub const DIFF: &str = "diff";
/// Top-level entries of the branch that are not a platform of the latest run.
const NOT_A_PLATFORM: [&str; 3] = [".git", BASELINE, DIFF];

/// What the gate writes beside its captures; published so a missing screen's reason (a
/// `GATE NOTE`) is one click from the gallery.
const REPORTS: [&str; 2] = ["gate.txt", "tree-gate.txt"];

/// One run's captures: platform → screen name → the variants (`en-100`, …) it was taken in.
pub type Screens = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;

/// One capture: `(platform, name, variant)`.
pub type Key = (String, String, String);

/// The latest run against the baseline.
#[derive(Debug, Default)]
pub struct Report {
    /// The run `baseline/` was approved from, when there is a baseline.
    pub baseline: Option<RunInfo>,
    pub changed: Vec<(Key, Outcome)>,
    /// Captures the baseline does not have.
    pub added: Vec<Key>,
    /// Baseline captures this run did not take.
    pub missing: Vec<Key>,
}

impl Report {
    /// The changed captures by screen: `(platform, name)` → variants.
    fn changed_screens(&self) -> BTreeMap<(String, String), Vec<(String, Outcome)>> {
        let mut out: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for ((platform, name, variant), found) in &self.changed {
            out.entry((platform.clone(), name.clone()))
                .or_default()
                .push((variant.clone(), *found));
        }
        for list in out.values_mut() {
            list.sort_by_key(|(v, _)| variant_order(v));
        }
        out
    }
}

/// A capture's or variant's name is lower-case letters, digits and dashes: it becomes a path
/// and a Markdown link, so nothing else gets through.
pub fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Replace the run in `site` with the artifacts in `shots`, compare it with the baseline and
/// write the gallery. Returns the job summary and the report.
pub fn publish(
    shots: &Path,
    site: &Path,
    info: &RunInfo,
    opt: Options,
) -> io::Result<(String, Report)> {
    clear_run(site)?;
    copy_run(shots, site)?;
    finish(site, info, opt)
}

/// Make the latest run in `site` the baseline, refusing when it is not the run `expect`
/// (a commit SHA or its prefix) names. Returns the job summary.
pub fn approve(site: &Path, expect: Option<&str>, opt: Options) -> io::Result<String> {
    let info = read_run(&site.join("run.txt"))?;
    if let Some(want) = expect.filter(|w| !w.is_empty())
        && !info.sha.starts_with(want)
    {
        return Err(io::Error::other(format!(
            "the gallery shows {}, not {want}: refresh it, or approve what it shows",
            info.sha
        )));
    }
    let run = scan(site, &NOT_A_PLATFORM)?;
    if run.is_empty() {
        return Err(io::Error::other("the branch holds no run to approve"));
    }
    let baseline = site.join(BASELINE);
    if baseline.exists() {
        std::fs::remove_dir_all(&baseline)?;
    }
    for (platform, by_name) in &run {
        for (name, variants) in by_name {
            for variant in variants {
                let from = capture_path(site, platform, variant, name);
                let to = capture_path(&baseline, platform, variant, name);
                std::fs::create_dir_all(to.parent().unwrap_or(baseline.as_path()))?;
                std::fs::copy(from, to)?;
            }
        }
    }
    std::fs::write(baseline.join("approved.txt"), run_txt(&info))?;
    let (summary, _) = finish(site, &info, opt)?;
    Ok(summary)
}

/// Compare the run in `site` with its baseline; write the highlights, `run.txt` and
/// `index.md`.
fn finish(site: &Path, info: &RunInfo, opt: Options) -> io::Result<(String, Report)> {
    let run = scan(site, &NOT_A_PLATFORM)?;
    let base_dir = site.join(BASELINE);
    let base = scan(&base_dir, &[])?;
    let diffs = site.join(DIFF);
    if diffs.exists() {
        std::fs::remove_dir_all(&diffs)?;
    }
    let mut report = Report {
        baseline: read_run(&base_dir.join("approved.txt")).ok(),
        ..Report::default()
    };
    for (key, new) in captures(&run, site) {
        let (platform, name, variant) = &key;
        let old = capture_path(&base_dir, platform, variant, name);
        if !old.exists() {
            if report.baseline.is_some() {
                report.added.push(key);
            }
            continue;
        }
        let found = diff::compare_files(
            &old,
            &new,
            &capture_path(&diffs, platform, variant, name),
            opt,
        )?;
        if found.changed {
            report.changed.push((key, found));
        }
    }
    for (key, _) in captures(&base, &base_dir) {
        let (platform, name, variant) = &key;
        if !capture_path(site, platform, variant, name).exists() {
            report.missing.push(key);
        }
    }
    std::fs::write(site.join("run.txt"), run_txt(info))?;
    std::fs::write(site.join("index.md"), index(&run, &report, info))?;
    Ok((summary(&run, &report, info), report))
}

fn capture_path(root: &Path, platform: &str, variant: &str, name: &str) -> PathBuf {
    root.join(platform)
        .join(variant)
        .join(format!("{name}.png"))
}

/// Every capture of `screens`, with its file under `root`.
fn captures(screens: &Screens, root: &Path) -> Vec<(Key, PathBuf)> {
    let mut out = Vec::new();
    for (platform, by_name) in screens {
        for (name, variants) in by_name {
            for variant in variants {
                let path = capture_path(root, platform, variant, name);
                out.push(((platform.clone(), name.clone(), variant.clone()), path));
            }
        }
    }
    out
}

/// Everything but the git metadata and the baseline goes: the branch holds one run.
fn clear_run(site: &Path) -> io::Result<()> {
    std::fs::create_dir_all(site)?;
    for entry in std::fs::read_dir(site)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || name == BASELINE {
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
/// `<platform>/<variant>/`.
fn copy_run(shots: &Path, site: &Path) -> io::Result<()> {
    for artifact in sorted_dirs(shots)? {
        let Some(platform) = file_name(&artifact)
            .strip_prefix("gui-gate-")
            .map(str::to_string)
            .filter(|p| safe_name(p) && !NOT_A_PLATFORM.contains(&p.as_str()))
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
                let png = file.strip_suffix(".png").is_some_and(safe_name);
                if png || REPORTS.contains(&file.as_str()) {
                    std::fs::create_dir_all(&out)?;
                    std::fs::copy(&path, out.join(&file))?;
                }
            }
        }
    }
    Ok(())
}

/// The captures under `root`'s platform folders (every top-level folder but `skip`).
pub fn scan(root: &Path, skip: &[&str]) -> io::Result<Screens> {
    let mut screens = Screens::new();
    if !root.is_dir() {
        return Ok(screens);
    }
    for platform_dir in sorted_dirs(root)? {
        let platform = file_name(&platform_dir);
        if skip.contains(&platform.as_str()) || !safe_name(&platform) {
            continue;
        }
        for variant_dir in sorted_dirs(&platform_dir)? {
            let variant = file_name(&variant_dir);
            if !safe_name(&variant) {
                continue;
            }
            for entry in std::fs::read_dir(&variant_dir)? {
                let file = file_name(&entry?.path());
                if let Some(name) = file.strip_suffix(".png").filter(|n| safe_name(n)) {
                    screens
                        .entry(platform.clone())
                        .or_default()
                        .entry(name.to_string())
                        .or_default()
                        .insert(variant.clone());
                }
            }
        }
    }
    Ok(screens)
}

fn sorted_dirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
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

/// Every variant any screen of a platform was taken in, in display order.
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

/// Read back what [`run_txt`] wrote.
pub fn read_run(path: &Path) -> io::Result<RunInfo> {
    let text = std::fs::read_to_string(path)?;
    let mut info = RunInfo::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let slot = match k {
            "repo" => &mut info.repo,
            "branch" => &mut info.branch,
            "sha" => &mut info.sha,
            "date" => &mut info.date,
            "run" => &mut info.run,
            _ => continue,
        };
        *slot = v.to_string();
    }
    if info.sha.is_empty() {
        return Err(io::Error::other(format!("{}: no sha", path.display())));
    }
    Ok(info)
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// The heading line every page and summary opens with.
pub fn header(info: &RunInfo) -> String {
    format!(
        "`{}` at [`{}`](https://github.com/{}/commit/{}) · {} · [CI run]({})\n",
        info.branch,
        short(&info.sha),
        info.repo,
        info.sha,
        info.date,
        info.run
    )
}

fn baseline_line(report: &Report) -> String {
    match &report.baseline {
        Some(b) => format!(
            "Compared with the approved baseline: `{}` at `{}`, {}.\n",
            b.branch,
            short(&b.sha),
            b.date
        ),
        None => "No approved baseline yet: run the **ui-approve** workflow to approve this run.\n"
            .into(),
    }
}

/// A thumbnail linking to the full-size capture at `path` (relative to the branch root).
pub fn thumb(path: &str, width: u32) -> String {
    format!("<a href=\"{path}\"><img src=\"{path}\" width=\"{width}\"></a>")
}

fn changed_section(out: &mut String, report: &Report) {
    let screens = report.changed_screens();
    let _ = writeln!(
        out,
        "## Changed since the baseline: {} screens, {} captures\n",
        screens.len(),
        report.changed.len()
    );
    if screens.is_empty() {
        out.push_str("Nothing changed.\n\n");
        return;
    }
    out.push_str(
        "Red: changed pixels. Yellow: anti-aliasing, ignored. Magenta: area only one capture has.\n\n",
    );
    for ((platform, name), list) in &screens {
        let _ = writeln!(out, "### {} · {name}\n", platform_title(platform));
        out.push_str("| | Baseline | New | Difference |\n|---|---|---|---|\n");
        for (variant, found) in list {
            let what = if found.size_changed {
                "size changed".to_string()
            } else {
                format!("{} px", found.pixels)
            };
            let _ = writeln!(
                out,
                "| {variant}<br>{what} | {} | {} | {} |",
                thumb(&format!("{BASELINE}/{platform}/{variant}/{name}.png"), 300),
                thumb(&format!("{platform}/{variant}/{name}.png"), 300),
                thumb(&format!("{DIFF}/{platform}/{variant}/{name}.png"), 300),
            );
        }
        out.push('\n');
    }
}

// `(platform, name)` → variants, for a list of captures.
fn by_screen(keys: &[Key]) -> BTreeMap<(String, String), Vec<String>> {
    let mut out: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (platform, name, variant) in keys {
        out.entry((platform.clone(), name.clone()))
            .or_default()
            .push(variant.clone());
    }
    for list in out.values_mut() {
        list.sort_by_key(|v| variant_order(v));
    }
    out
}

fn list_section(out: &mut String, title: &str, keys: &[Key], root: &str) {
    if keys.is_empty() {
        return;
    }
    let _ = writeln!(out, "## {title}: {} captures\n", keys.len());
    for ((platform, name), variants) in by_screen(keys) {
        let first = &variants[0];
        let _ = writeln!(
            out,
            "- {} · {name} ({}) {}",
            platform_title(&platform),
            variants.join(", "),
            thumb(&format!("{root}{platform}/{first}/{name}.png"), 160)
        );
    }
    out.push('\n');
}

/// Every screen of every platform: one table per screen, one column per variant.
pub fn all_screens(out: &mut String, screens: &Screens) {
    for (platform, by_name) in screens {
        let variants = variants(by_name);
        let _ = writeln!(out, "### {}\n", platform_title(platform));
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
            let _ = writeln!(out, "#### {name}\n");
            let _ = writeln!(out, "| {} |", variants.join(" | "));
            let _ = writeln!(out, "|{}", "---|".repeat(variants.len()));
            let cells: Vec<String> = variants
                .iter()
                .map(|v| {
                    if have.contains(v) {
                        thumb(&format!("{platform}/{v}/{name}.png"), 220)
                    } else {
                        "—".into()
                    }
                })
                .collect();
            let _ = writeln!(out, "| {} |\n", cells.join(" | "));
        }
    }
}

fn index(run: &Screens, report: &Report, info: &RunInfo) -> String {
    let mut out = String::from("# freemkv desktop UI screenshots\n\n");
    out.push_str(&header(info));
    out.push('\n');
    out.push_str(&baseline_line(report));
    out.push('\n');
    if report.baseline.is_some() {
        changed_section(&mut out, report);
        list_section(
            &mut out,
            "New screens, not in the baseline",
            &report.added,
            "",
        );
        let base = format!("{BASELINE}/");
        list_section(&mut out, "Missing from this run", &report.missing, &base);
    }
    out.push_str("## All screens\n\n");
    all_screens(&mut out, run);
    out
}

fn summary(run: &Screens, report: &Report, info: &RunInfo) -> String {
    let mut out = String::from("## UI screenshots\n\n");
    out.push_str(&header(info));
    out.push('\n');
    for (platform, by_name) in run {
        let captures: usize = by_name.values().map(BTreeSet::len).sum();
        let _ = writeln!(
            out,
            "- {}: {} screens, {captures} captures",
            platform_title(platform),
            by_name.len()
        );
    }
    out.push('\n');
    out.push_str(&baseline_line(report));
    if report.baseline.is_some() {
        let screens = report.changed_screens();
        let _ = writeln!(
            out,
            "\n**{} screens changed** ({} captures); {} captures new, {} missing.\n",
            screens.len(),
            report.changed.len(),
            report.added.len(),
            report.missing.len()
        );
        for ((platform, name), list) in screens.iter().take(60) {
            let variants: Vec<&str> = list.iter().map(|(v, _)| v.as_str()).collect();
            let _ = writeln!(
                out,
                "- {} · {name} ({})",
                platform_title(platform),
                variants.join(", ")
            );
        }
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
    use image::{Rgba, RgbaImage};

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
        let mut v = ["ar-100", "de-200", "en-200", "de-100", "en-100", "xx-100"];
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
        assert!(out.contains("### Linux"), "{out}");
        assert!(out.contains("| en-100 | de-100 |"), "{out}");
        assert!(out.contains("linux/en-100/about.png"), "{out}");
        assert!(out.contains("| — |"), "{out}");
    }

    #[test]
    fn run_txt_reads_back() {
        let dir = scratch("run-txt");
        let info = info("abc123");
        std::fs::write(dir.join("run.txt"), run_txt(&info)).unwrap();
        assert_eq!(read_run(&dir.join("run.txt")).unwrap(), info);
    }

    fn info(sha: &str) -> RunInfo {
        RunInfo {
            repo: "freemkv/freemkv".into(),
            branch: "dev".into(),
            sha: sha.into(),
            date: "2026-11-20 12:00 UTC".into(),
            run: "https://github.com/freemkv/freemkv/actions/runs/1".into(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ui-review-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // An artifact folder holding `name` in `variant`, white with a black block at `x`.
    fn shoot(shots: &Path, variant: &str, name: &str, x: u32) {
        let dir = shots.join("gui-gate-linux").join(variant);
        std::fs::create_dir_all(&dir).unwrap();
        let mut img = RgbaImage::from_pixel(60, 30, Rgba([255, 255, 255, 255]));
        for y in 5..20 {
            for dx in 0..10 {
                img.put_pixel(x + dx, y, Rgba([0, 0, 0, 255]));
            }
        }
        img.save(dir.join(format!("{name}.png"))).unwrap();
        std::fs::write(dir.join("gate.txt"), "GATE OK: 1 checks\n").unwrap();
    }

    #[test]
    fn publish_approve_publish_lists_only_what_changed() {
        let root = scratch("cycle");
        let (site, first, second) = (root.join("site"), root.join("a"), root.join("b"));
        shoot(&first, "en-100", "about", 5);
        shoot(&first, "en-100", "menu-file", 5);
        shoot(&first, "en-100", "rip-single", 5);
        let opt = Options::default();

        let (_, report) = publish(&first, &site, &info("1111"), opt).unwrap();
        assert!(report.baseline.is_none());
        assert!(site.join("linux/en-100/about.png").exists());
        assert!(site.join("linux/en-100/gate.txt").exists());
        let index = std::fs::read_to_string(site.join("index.md")).unwrap();
        assert!(index.contains("No approved baseline yet"), "{index}");

        approve(&site, Some("1111"), opt).unwrap();
        assert!(site.join("baseline/linux/en-100/about.png").exists());
        assert!(approve(&site, Some("2222"), opt).is_err());

        // The next run: about moved, menu-file is the same, rip-single is gone, shortcuts is new.
        shoot(&second, "en-100", "about", 30);
        shoot(&second, "en-100", "menu-file", 5);
        shoot(&second, "en-100", "shortcuts", 5);
        let (summary, report) = publish(&second, &site, &info("2222"), opt).unwrap();
        let keys: Vec<&str> = report.changed.iter().map(|(k, _)| k.1.as_str()).collect();
        assert_eq!(keys, ["about"]);
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.missing.len(), 1);
        assert!(site.join("diff/linux/en-100/about.png").exists());
        assert!(!site.join("diff/linux/en-100/menu-file.png").exists());
        assert!(!site.join("linux/en-100/rip-single.png").exists());
        assert!(site.join("baseline/linux/en-100/rip-single.png").exists());
        assert!(summary.contains("**1 screens changed**"), "{summary}");
        let index = std::fs::read_to_string(site.join("index.md")).unwrap();
        let changed = index.find("## Changed since the baseline").unwrap();
        let all = index.find("## All screens").unwrap();
        assert!(changed < all, "{index}");
        assert!(index.contains("diff/linux/en-100/about.png"), "{index}");

        // Approving the new run leaves nothing changed.
        approve(&site, None, opt).unwrap();
        let index = std::fs::read_to_string(site.join("index.md")).unwrap();
        assert!(index.contains("Nothing changed."), "{index}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
