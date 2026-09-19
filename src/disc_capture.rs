//! `freemkv info … --share` — disc-structure capture for bug reports.
//!
//! MIT — freemkv project. CLI is dumb; all disc reads come from libfreemkv.
//!
//! Companion to the drive-profile capture in `info.rs`: this packages a disc's
//! small BDMV/VIDEO_TS *metadata* (playlists, clip info, nav, DVD IFOs — no A/V
//! essence, no AACS keys) plus freemkv's own title-selection view, so a reporter
//! can reproduce a wrong-title / selection bug (e.g. issue #45) without shipping
//! the whole disc. Reuses the same zip + base64 + submit flow as the drive path.

use crate::info::{base64_encode, present_for_submission, save_bin, zip_files};
use crate::strings;
use libfreemkv::{Disc, SectorSource};
use std::path::Path;

// What the disc capture added to a profile, for the summary line + issue body.
pub(crate) struct DiscSummary {
    pub file_count: usize,
    pub total_bytes: usize,
}

// Read the disc's structure files and save them (nested) into `profile_dir`,
// appending each relative path to `written`. Best-effort: any read failure
// returns `None` so a drive-path caller with other data still builds a profile.
pub(crate) fn fold_structure(
    profile_dir: &Path,
    written: &mut Vec<String>,
    reader: &mut dyn SectorSource,
) -> Option<DiscSummary> {
    let files = Disc::read_structure_files(reader).ok()?;
    if files.is_empty() {
        return None;
    }
    let total_bytes = files.iter().map(|(_, b)| b.len()).sum();
    let file_count = files.len();
    for (rel, bytes) in files {
        save_bin(profile_dir, &rel, &bytes, written);
    }
    Some(DiscSummary {
        file_count,
        total_bytes,
    })
}

// A JSON view of freemkv's decision so a maintainer sees the selection without
// re-running: picked title first (`titles[0]`), then every title's shape.
// Literal English/JSON — a machine artifact, not localized UI.
pub(crate) fn selection_json(disc: &Disc) -> String {
    fn esc(s: &str) -> String {
        s.chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32).chars().collect(),
                c => vec![c],
            })
            .collect()
    }
    let pick = disc.titles.first().map(|t| t.playlist_id).unwrap_or(0);
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!(
        "  \"freemkv\": \"{}\",\n",
        esc(libfreemkv::VERSION_LABEL)
    ));
    s.push_str(&format!("  \"format\": \"{:?}\",\n", disc.format));
    s.push_str(&format!("  \"picked_playlist_id\": {pick},\n"));
    s.push_str(&format!("  \"title_count\": {},\n", disc.titles.len()));
    s.push_str("  \"titles\": [\n");
    for (i, t) in disc.titles.iter().enumerate() {
        let clips: Vec<String> = t
            .clips
            .iter()
            .map(|c| format!("\"{}\"", esc(&c.clip_id)))
            .collect();
        s.push_str(&format!(
            "    {{\"playlist_id\": {}, \"playlist\": \"{}\", \"duration_secs\": {:.1}, \
             \"size_bytes\": {}, \"has_video\": {}, \"has_probable_video\": {}, \
             \"clips\": [{}]}}{}\n",
            t.playlist_id,
            esc(&t.playlist),
            t.duration_secs,
            t.size_bytes,
            t.has_video(),
            t.has_probable_video(),
            clips.join(", "),
            if i + 1 < disc.titles.len() { "," } else { "" },
        ));
    }
    s.push_str("  ]\n}\n");
    s
}

// Full `--share` flow for an ISO / folder / already-scanned disc (no drive):
// write structure + selection into a profile dir, zip it, print saved paths plus
// a paste/email base64 bundle. Exits on a hard I/O failure, like the drive path.
pub(crate) fn run(disc: &Disc, reader: &mut dyn SectorSource, label: &str) {
    let profile_name = format!("disc-profile-{}", crate::info::sanitize_component(label));
    let profile_dir = std::path::PathBuf::from(&profile_name);
    if let Err(e) = std::fs::create_dir_all(&profile_dir) {
        eprintln!(
            "{}",
            strings::fmt_or(
                "disc.capture_mkdir_failed",
                "Could not create {path}: {error}",
                &[
                    ("path", &profile_dir.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        );
        std::process::exit(1);
    }

    let mut written: Vec<String> = Vec::new();

    // freemkv's selection view (always available — we have the scanned disc).
    save_bin(
        &profile_dir,
        "selection.json",
        selection_json(disc).as_bytes(),
        &mut written,
    );

    // Raw structure files. If none are readable there is nothing to report.
    let summary = match fold_structure(&profile_dir, &mut written, reader) {
        Some(s) => s,
        None => {
            eprintln!(
                "{}",
                strings::get_or(
                    "disc.capture_no_structure",
                    "No readable disc structure (BDMV / VIDEO_TS) was found; nothing to share.",
                )
            );
            std::process::exit(1);
        }
    };

    println!(
        "{}",
        strings::fmt_or(
            "disc.capture_summary",
            "Captured disc structure: {files} files ({bytes} bytes).",
            &[
                ("files", &summary.file_count.to_string()),
                ("bytes", &summary.total_bytes.to_string()),
            ],
        )
    );

    // Zip + base64, same helpers as the drive path.
    let zip_data = match zip_files(&profile_dir, &written) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "{}",
                strings::fmt_or(
                    "drive.zip_failed",
                    "Could not build zip: {error}",
                    &[("error", &e.to_string())]
                )
            );
            std::process::exit(1);
        }
    };
    let zip_path = profile_dir.join("profile.zip");
    if let Err(e) = std::fs::write(&zip_path, &zip_data) {
        eprintln!(
            "{}",
            strings::fmt(
                "error.cannot_write",
                &[
                    ("path", &zip_path.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        );
        std::process::exit(1);
    }

    let body = issue_body(disc, &summary, &base64_encode(&zip_data));
    let title = format!(
        "Disc profile: {:?} ({} titles)",
        disc.format,
        disc.titles.len()
    );
    present_for_submission(&profile_name, &zip_path, &title, &body);
}

// The GitHub issue / email body. Literal English markdown (a machine artifact,
// like the drive body): a disc summary a human can read without unzipping, then
// the base64 zip in a <details> block.
pub(crate) fn issue_body(disc: &Disc, summary: &DiscSummary, zip_b64: &str) -> String {
    let pick = disc.titles.first();
    let mut body = String::new();
    body.push_str("## Disc structure\n\n```\n");
    body.push_str(&format!("Format:          {:?}\n", disc.format));
    body.push_str(&format!("Titles:          {}\n", disc.titles.len()));
    if let Some(p) = pick {
        body.push_str(&format!(
            "Selected title:  playlist {} ({}), {:.0}s, {} bytes\n",
            p.playlist_id,
            crate::disc_info::sanitize(&p.playlist),
            p.duration_secs,
            p.size_bytes,
        ));
    }
    body.push_str(&format!(
        "Structure files: {} ({} bytes)\n",
        summary.file_count, summary.total_bytes
    ));
    body.push_str("```\n\n");
    body.push_str(
        "Metadata only — no audio/video essence, no AACS keys. `selection.json` in \
         the zip shows freemkv's full title ranking.\n\n",
    );
    body.push_str("<details><summary>Disc structure (base64 zip)</summary>\n\n```\n");
    for chunk in zip_b64.as_bytes().chunks(76) {
        body.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        body.push('\n');
    }
    body.push_str("```\n\n</details>\n\n");
    body.push_str("---\n*Captured by `freemkv info … --share`*\n");
    body
}
