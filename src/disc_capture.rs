//! `freemkv info … --share` — disc-structure capture for bug reports.
//!
//! MIT — freemkv project. CLI is dumb; all disc reads come from libfreemkv.
//!
//! Companion to the drive-profile capture in `info.rs`: this packages a disc's
//! small BDMV/VIDEO_TS *metadata* (playlists, clip info, nav, DVD IFOs — no A/V
//! essence, no AACS keys) plus freemkv's own title-selection view, so a reporter
//! can reproduce a wrong-title / selection bug (e.g. issue #45) without shipping
//! the whole disc. Reuses the same zip + base64 + submit flow as the drive path.
//!
//! It also bundles `aacs.json` — non-secret AACS diagnostics for keydb / AACS
//! resolution triage (issue #46): the computed disc hash (the keydb lookup key),
//! AACS generation, MKB version, bus-encryption flag, and a vid-available
//! boolean. NEVER any key material — no VUK, unit keys, MKB bytes, or raw VID.
//! When there is no AACS state it records why: the AACS step failed (its error
//! code), or the disc has no AACS at all (DVD / unencrypted Blu-ray).

use crate::info::{
    base64_encode, json_escape, present_for_submission, push_zip_section, try_save_bin, zip_files,
};
use crate::strings;
use libfreemkv::{Disc, SectorSource};
use std::path::Path;

// What the disc capture added to a profile, for the summary line + issue body.
pub(crate) struct DiscSummary {
    pub file_count: usize,
    pub total_bytes: usize,
    // Localized "name: reason" lines for disc files that were not saved.
    pub skipped: Vec<String>,
}

// Save the disc's structure files (nested) into `profile_dir`, recording each in
// `written`. `Ok(None)` = no files; `Err` = the read failure. Best-effort per file:
// an unsafe name or failed write skips that file only (see `DiscSummary::skipped`).
pub(crate) fn fold_structure(
    profile_dir: &Path,
    written: &mut Vec<String>,
    reader: &mut dyn SectorSource,
) -> Result<Option<DiscSummary>, libfreemkv::Error> {
    let files = Disc::read_structure_files(reader)?;
    if files.is_empty() {
        return Ok(None);
    }
    let mut summary = DiscSummary {
        file_count: 0,
        total_bytes: 0,
        skipped: Vec::new(),
    };
    for (rel, bytes) in files {
        let shown = strings::sanitize_display(&rel);
        let saved = if is_safe_rel_path(&rel) {
            try_save_bin(profile_dir, &rel, &bytes).map_err(|e| e.to_string())
        } else {
            Err(strings::get_or(
                "disc.capture_unsafe_name",
                "unsafe file name",
            ))
        };
        match saved {
            Ok(()) => {
                written.push(rel);
                summary.file_count += 1;
                summary.total_bytes += bytes.len();
            }
            Err(reason) => summary.skipped.push(format!("{shown}: {reason}")),
        }
    }
    Ok(Some(summary))
}

// Whether a disc-supplied relative path is safe to join under the profile dir on
// every OS: `/`-separated components, each passing `is_plain_component`.
pub(crate) fn is_safe_rel_path(rel: &str) -> bool {
    !rel.is_empty() && rel.split('/').all(is_plain_component)
}

// One path component safe on any host, incl. Windows. Mirrors libfreemkv dev's
// `is_plain_file_name`, which the pinned v1.7.7 tag lacks.
fn is_plain_component(name: &str) -> bool {
    if name.is_empty() || name.ends_with('.') || name.ends_with(' ') {
        return false; // also rejects "." and ".."
    }
    if name.chars().any(|c| {
        matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') || c.is_control()
    }) {
        return false;
    }
    // Windows reserved device names, matched on the stem with trailing spaces ignored.
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let stem = stem.to_ascii_uppercase();
    let numbered = |p: &str| {
        stem.strip_prefix(p).is_some_and(|d| {
            let mut c = d.chars();
            matches!(
                (c.next(), c.next()),
                (Some('0'..='9' | '\u{B9}' | '\u{B2}' | '\u{B3}'), None)
            )
        })
    };
    !(matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT"))
}

// Every structure file was found but none could be saved.
pub(crate) fn all_skipped_line(summary: &DiscSummary) -> String {
    strings::fmt_or(
        "disc.capture_all_skipped",
        "None of the {count} disc structure files could be saved.",
        &[("count", &summary.skipped.len().to_string())],
    )
}

// Tell the user which disc files were left out of the profile, and why.
pub(crate) fn report_skipped(summary: &DiscSummary) {
    for line in skipped_lines(summary) {
        eprintln!("{line}");
    }
}

// The localized "Skipped disc file …" lines `report_skipped` prints.
fn skipped_lines(summary: &DiscSummary) -> Vec<String> {
    summary
        .skipped
        .iter()
        .map(|line| {
            strings::fmt_or(
                "disc.capture_skipped",
                "Skipped disc file {file}",
                &[("file", line)],
            )
        })
        .collect()
}

/// Non-secret AACS diagnostics distilled from a scanned disc, for keydb / AACS
/// resolution triage (issue #46). Deliberately omits EVERY secret: no VUK, no
/// unit keys, no raw Volume ID, no MKB / Unit_Key_RO bytes — only crypto *shape*
/// plus the disc hash, which is the public keydb lookup key (SHA-1 of
/// `Unit_Key_RO.inf`, the same value a maintainer matches against keydb rows).
pub(crate) struct AacsDiag {
    // Whether the scan carried any AACS state at all.
    pub captured: bool,
    // Uncaptured because the AACS step failed: its language-neutral `E<code>`.
    pub error_code: Option<String>,
    // KEYDB.cfg lookup key in its row form, `0x<40hex>`, when the scan read the
    // disc's `Unit_Key_RO.inf`. `None` when uncaptured or the hash was blank.
    pub disc_hash: Option<String>,
    // AACS generation / major version (1 = BD, 2 = UHD).
    pub generation: Option<u8>,
    pub mkb_version: Option<u32>,
    pub bus_encryption: Option<bool>,
    // Whether the SCSI AACS handshake yielded a Volume ID. The raw VID is
    // NEVER emitted — only this boolean.
    pub vid_available: bool,
    // The disc is encrypted but the scan carried neither AACS state nor an AACS failure (a
    // scan without keys): the state was not read, which is not "no AACS".
    pub unread: bool,
}

// Distil the non-secret AACS diagnostics from a scanned disc. Reads only
// `disc.aacs` shape; never touches vuk / unit_keys / uk_ro / mkb bytes.
pub(crate) fn aacs_diag(disc: &Disc) -> AacsDiag {
    match disc.aacs.as_ref() {
        Some(a) => {
            // KEYDB.cfg rows are keyed `0x<hash>`; normalise to exactly that form.
            let hash = libfreemkv::hex::strip_hex_prefix(a.disc_hash.trim()).trim();
            AacsDiag {
                captured: true,
                error_code: None,
                disc_hash: (!hash.is_empty()).then(|| format!("0x{hash}")),
                generation: Some(a.version),
                mkb_version: a.mkb_version,
                bus_encryption: Some(a.bus_encryption),
                // Raw VID stays private — report only whether one was obtained.
                vid_available: a.volume_id.iter().any(|&b| b != 0),
                unread: false,
            }
        }
        None => AacsDiag {
            captured: false,
            error_code: disc.aacs_error.as_ref().map(|e| format!("E{}", e.code())),
            disc_hash: None,
            generation: None,
            mkb_version: None,
            bus_encryption: None,
            vid_available: false,
            unread: disc.encrypted && disc.aacs_error.is_none(),
        },
    }
}

// Why no AACS diagnostics exist, for the English machine artifacts (JSON note + issue body).
fn aacs_absent_reason(d: &AacsDiag) -> String {
    match &d.error_code {
        Some(code) => format!("Not captured — the AACS step failed ({code})."),
        None if d.unread => "Not captured — the disc is encrypted but was scanned without keys, \
             so its AACS state was not read."
            .to_string(),
        None => "No AACS on this disc (DVD or unencrypted Blu-ray).".to_string(),
    }
}

/// The `aacs.json` profile member: non-secret AACS diagnostics for keydb triage.
/// Without AACS state it records why (failed step's code, or no AACS on the
/// disc). NEVER emits key material, VUK, unit keys, MKB bytes, or the raw
/// Volume ID — only shape + the public disc hash. Literal English/JSON.
pub(crate) fn aacs_json(disc: &Disc) -> String {
    let d = aacs_diag(disc);
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!(
        "  \"aacs_present\": {},\n",
        d.captured || d.error_code.is_some() || d.unread
    ));
    s.push_str(&format!("  \"aacs_captured\": {},\n", d.captured));
    if d.captured {
        match &d.disc_hash {
            Some(h) => s.push_str(&format!("  \"disc_hash\": \"{}\",\n", json_escape(h))),
            None => s.push_str("  \"disc_hash\": null,\n"),
        }
        match d.generation {
            Some(g) => s.push_str(&format!("  \"aacs_generation\": {g},\n")),
            None => s.push_str("  \"aacs_generation\": null,\n"),
        }
        match d.mkb_version {
            Some(v) => s.push_str(&format!("  \"mkb_version\": {v},\n")),
            None => s.push_str("  \"mkb_version\": null,\n"),
        }
        s.push_str(&format!(
            "  \"bus_encryption\": {},\n",
            d.bus_encryption.unwrap_or(false)
        ));
        s.push_str(&format!("  \"vid_available\": {},\n", d.vid_available));
        s.push_str(
            "  \"note\": \"disc_hash is the AACS keydb lookup key (SHA-1 of Unit_Key_RO.inf) in \
             KEYDB.cfg's `0x<hash>` row form. Present only when the scan read the disc's AACS data. \
             No key material, VUK, unit keys, MKB bytes, or raw Volume ID is included — \
             vid_available only reports whether the SCSI handshake yielded a Volume ID.\"\n",
        );
    } else {
        if let Some(code) = &d.error_code {
            s.push_str(&format!("  \"aacs_error\": \"{}\",\n", json_escape(code)));
        }
        s.push_str(&format!(
            "  \"note\": \"{}\"\n",
            json_escape(&aacs_absent_reason(&d))
        ));
    }
    s.push_str("}\n");
    s
}

// A JSON view of freemkv's decision so a maintainer sees the selection without
// re-running: picked title first (`titles[0]`), then every title's shape.
// Literal English/JSON — a machine artifact, not localized UI.
pub(crate) fn selection_json(disc: &Disc) -> String {
    let esc = json_escape;
    let pick = match disc.titles.first() {
        Some(t) => t.playlist_id.to_string(),
        None => "null".to_string(),
    };
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!(
        "  \"freemkv\": \"{}\",\n",
        esc(env!("CARGO_PKG_VERSION"))
    ));
    s.push_str(&format!(
        "  \"libfreemkv\": \"{}\",\n",
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
pub(crate) fn run(disc: &Disc, reader: &mut dyn SectorSource, label: &str, quiet: bool) {
    let profile_name = format!("disc-profile-{}", crate::info::sanitize_component(label));
    match capture(disc, reader, Path::new(&profile_name), quiet) {
        Ok(c) => {
            c.stderr.iter().for_each(|l| eprintln!("{l}"));
            c.stdout.iter().for_each(|l| println!("{l}"));
            present_for_submission(&profile_name, &c.zip_path, &c.title, &c.body, c.inlined);
        }
        Err(lines) => {
            lines.iter().for_each(|l| eprintln!("{l}"));
            crate::cli_entry::exit(1);
        }
    }
}

// One `--share` capture: the console lines `run` prints (stderr first, as the
// flow emits them) and the issue it hands to `present_for_submission`.
pub(crate) struct Capture {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub zip_path: std::path::PathBuf,
    pub title: String,
    pub body: String,
    pub inlined: bool,
}

// Write the profile into `profile_dir` and build the submission, printing
// nothing. `Err` holds the stderr lines of a hard failure (`run` exits 1).
pub(crate) fn capture(
    disc: &Disc,
    reader: &mut dyn SectorSource,
    profile_dir: &Path,
    quiet: bool,
) -> Result<Capture, Vec<String>> {
    let cannot_write = |path: &Path, e: &dyn std::fmt::Display| {
        vec![strings::fmt(
            "error.cannot_write",
            &[
                ("path", &path.display().to_string()),
                ("error", &e.to_string()),
            ],
        )]
    };
    if let Err(e) = std::fs::create_dir_all(profile_dir) {
        return Err(vec![strings::fmt_or(
            "disc.capture_mkdir_failed",
            "Could not create {path}: {error}",
            &[
                ("path", &profile_dir.display().to_string()),
                ("error", &e.to_string()),
            ],
        )]);
    }

    let mut written: Vec<String> = Vec::new();
    // freemkv's selection view, then the non-secret AACS diagnostics for keydb
    // triage (issue #46; never key material, see `aacs_json`).
    for (name, data) in [
        ("selection.json", selection_json(disc)),
        ("aacs.json", aacs_json(disc)),
    ] {
        try_save_bin(profile_dir, name, data.as_bytes())
            .map_err(|e| cannot_write(&profile_dir.join(name), &e))?;
        written.push(name.to_string());
    }

    // Raw structure files. If none are readable there is nothing to report.
    let mut stderr = Vec::new();
    let summary = match fold_structure(profile_dir, &mut written, reader) {
        Ok(Some(s)) if s.file_count > 0 => {
            if !quiet {
                stderr.extend(skipped_lines(&s));
            }
            s
        }
        // Every file was refused: the reasons ARE the error, so show them even under -q.
        Ok(Some(s)) => {
            let mut lines = skipped_lines(&s);
            lines.push(all_skipped_line(&s));
            return Err(lines);
        }
        other => {
            let mut lines = vec![strings::get_or(
                "disc.capture_no_structure",
                "No readable disc structure (BDMV / VIDEO_TS) was found; nothing to share.",
            )];
            if let Err(e) = other {
                lines.push(format!("  {}", crate::pipe::fmt_err(&e)));
            }
            return Err(lines);
        }
    };
    let stdout = if quiet {
        Vec::new()
    } else {
        capture_notes(disc, &summary)
    };

    // Zip + base64, same helpers as the drive path.
    let zip_data =
        zip_files(profile_dir, &written).map_err(|e| vec![crate::info::zip_failed_line(&*e)])?;
    let zip_path = profile_dir.join("profile.zip");
    std::fs::write(&zip_path, &zip_data).map_err(|e| cannot_write(&zip_path, &e))?;

    let (body, inlined) = issue_body(disc, &summary, &base64_encode(&zip_data));
    let title = format!(
        "Disc profile: {:?} ({} titles)",
        disc.format,
        disc.titles.len()
    );
    Ok(Capture {
        stdout,
        stderr,
        zip_path,
        title,
        body,
        inlined,
    })
}

// The on-run summary + AACS triage lines (localized; suppressed by `-q`).
fn capture_notes(disc: &Disc, summary: &DiscSummary) -> Vec<String> {
    let files = strings::fmt_or(
        "disc.capture_summary",
        "Captured disc structure: {files} files ({bytes} bytes).",
        &[
            ("files", &summary.file_count.to_string()),
            ("bytes", &summary.total_bytes.to_string()),
        ],
    );
    let diag = aacs_diag(disc);
    let line = match (&diag.disc_hash, &disc.aacs_error) {
        (Some(hash), _) => strings::fmt_or(
            "disc.capture_aacs_hash",
            "AACS diagnostics: disc hash {hash} (keydb lookup key) recorded in aacs.json.",
            &[("hash", hash)],
        ),
        (None, _) if diag.captured => strings::get_or(
            "disc.capture_aacs_no_hash",
            "AACS diagnostics recorded in aacs.json (disc hash unavailable).",
        ),
        (None, Some(e)) => strings::fmt_or(
            "disc.capture_aacs_failed",
            "AACS diagnostics not captured — the AACS step failed: {error}",
            &[("error", &crate::pipe::fmt_err(e))],
        ),
        (None, None) if diag.unread => strings::get_or(
            "disc.capture_aacs_unread",
            "AACS diagnostics not captured — the disc is encrypted but was scanned without keys.",
        ),
        (None, None) => strings::get_or(
            "disc.capture_aacs_none",
            "No AACS on this disc — no AACS diagnostics to capture.",
        ),
    };
    vec![files, line]
}

// The GitHub issue / email body (literal English markdown, a machine artifact): a
// readable disc summary, then the base64 zip — or an attach note when too large.
// Returns the body and whether the zip was inlined.
pub(crate) fn issue_body(disc: &Disc, summary: &DiscSummary, zip_b64: &str) -> (String, bool) {
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

    // AACS diagnostics (issue #46): the disc hash (keydb lookup key) + crypto
    // shape, or why there is none. Non-secret only; see `aacs_json`.
    let diag = aacs_diag(disc);
    body.push_str("## AACS diagnostics\n\n```\n");
    if diag.captured {
        match &diag.disc_hash {
            Some(h) => body.push_str(&format!("Disc hash:       {h}  (keydb lookup key)\n")),
            None => body.push_str("Disc hash:       (unavailable)\n"),
        }
        if let Some(g) = diag.generation {
            body.push_str(&format!("AACS generation: {g}\n"));
        }
        match diag.mkb_version {
            Some(v) => body.push_str(&format!("MKB version:     {v}\n")),
            None => body.push_str("MKB version:     (unknown)\n"),
        }
        body.push_str(&format!(
            "Bus encryption:  {}\n",
            diag.bus_encryption.unwrap_or(false)
        ));
        body.push_str(&format!("VID available:   {}\n", diag.vid_available));
    } else {
        body.push_str(&aacs_absent_reason(&diag));
        body.push('\n');
    }
    body.push_str("```\n\n");

    body.push_str(
        "Metadata only — no audio/video essence, no AACS keys (no VUK, unit keys, MKB \
         bytes, or raw Volume ID). `selection.json` in the zip shows freemkv's full \
         title ranking; `aacs.json` carries the non-secret AACS diagnostics above \
         (disc hash, version, vid-available) for keydb triage.\n\n",
    );
    let inlined = push_zip_section(
        &mut body,
        "Disc structure (base64 zip)",
        zip_b64,
        "---\n*Captured by `freemkv info … --share`*\n",
    );
    (body, inlined)
}

#[cfg(test)]
#[path = "disc_capture_aacs_diag_tests.rs"]
mod aacs_diag_tests;

#[cfg(test)]
#[path = "disc_capture_fold_structure_tests.rs"]
mod fold_structure_tests;

#[cfg(test)]
#[path = "disc_capture_share_fix_tests.rs"]
mod share_fix_tests;
