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
        },
    }
}

// Why no AACS diagnostics exist, for the English machine artifacts (JSON note + issue body).
fn aacs_absent_reason(d: &AacsDiag) -> String {
    match &d.error_code {
        Some(code) => format!("Not captured — the AACS step failed ({code})."),
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
        d.captured || d.error_code.is_some()
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
    let pick = disc.titles.first().map(|t| t.playlist_id).unwrap_or(0);
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
            std::process::exit(1);
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
mod aacs_diag_tests {
    use super::*;
    use libfreemkv::disc::DiscRegion;
    use libfreemkv::{AacsState, Disc, DiscFormat};

    // Distinctive secrets. The unit key is the rip's (it encrypts the fixture image),
    // never the disc's; the VID and MKB bytes sit in the scanned `AacsState`.
    const SECRET_UNIT_KEY: [u8; 16] = *b"\xC1unit-key-KU-P1!";
    const SECRET_VID: [u8; 16] = [0xEF; 16];
    const SECRET_MKB: [u8; 4] = [0x78, 0x9a, 0xbc, 0xde];

    fn disc_with(aacs: Option<AacsState>) -> Disc {
        Disc {
            volume_id: "TEST_DISC".to_string(),
            meta_title: None,
            format: DiscFormat::Uhd,
            capacity_sectors: 0,
            capacity_bytes: 0,
            layers: 1,
            titles: Vec::new(),
            region: DiscRegion::Free,
            aacs,
            css: None,
            encrypted: true,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    // A scanned AACS state: a real disc hash plus the known VID, MKB and
    // `Unit_Key_RO.inf` bytes, so artifacts can leak the hash but none of those.
    fn aacs_with_secrets(disc_hash: &str) -> AacsState {
        libfreemkv::test_util::aacs_state()
            .version(2)
            .bus_encryption(true)
            .mkb_version(Some(77))
            .disc_hash(disc_hash)
            .volume_id(SECRET_VID)
            .uk_ro(vec![0x12, 0x34, 0x56])
            .mkb(SECRET_MKB.to_vec())
            .build()
    }

    #[test]
    fn aacs_present_puts_disc_hash_into_profile() {
        // KEYDB.cfg rows are keyed `0x<40hex>`; the profile records that exact form.
        let raw = "0xaabbccddeeff00112233445566778899aabbccdd";
        let bare = "0xaabbccddeeff00112233445566778899aabbccdd";
        let disc = disc_with(Some(aacs_with_secrets(raw)));

        let json = aacs_json(&disc);
        assert!(json.contains("\"aacs_captured\": true"), "{json}");
        assert!(
            json.contains(&format!("\"disc_hash\": \"{bare}\"")),
            "disc hash must be the bare keydb lookup key: {json}"
        );
        assert!(json.contains("\"aacs_generation\": 2"), "{json}");
        assert!(json.contains("\"mkb_version\": 77"), "{json}");
        assert!(json.contains("\"bus_encryption\": true"), "{json}");
        // volume_id is non-zero, so a VID was available — but only the boolean.
        assert!(json.contains("\"vid_available\": true"), "{json}");

        let body = issue_body(
            &disc,
            &DiscSummary {
                file_count: 3,
                total_bytes: 100,
                skipped: Vec::new(),
            },
            "QUJD",
        )
        .0;
        assert!(body.contains("## AACS diagnostics"), "{body}");
        assert!(
            body.contains(bare),
            "issue body must name the disc hash: {body}"
        );
    }

    #[test]
    fn a_disc_with_no_aacs_state_records_no_crypto_shape() {
        let disc = disc_with(None);
        let json = aacs_json(&disc);
        assert!(json.contains("\"aacs_captured\": false"), "{json}");
        // No crypto-shape fields when nothing was captured (assert the JSON key form only).
        assert!(!json.contains("\"disc_hash\""), "{json}");
        assert!(!json.contains("\"vid_available\""), "{json}");
        let body = issue_body(
            &disc,
            &DiscSummary {
                file_count: 1,
                total_bytes: 10,
                skipped: Vec::new(),
            },
            "QUJD",
        )
        .0;
        assert!(body.contains("No AACS on this disc"), "{body}");
    }

    /// L10: the bundle names freemkv's own version, not libfreemkv's label.
    #[test]
    fn selection_json_names_the_freemkv_version() {
        let sel = selection_json(&disc_with(None));
        let want = format!("\"freemkv\": \"{}\",", env!("CARGO_PKG_VERSION"));
        assert!(sel.contains(&want), "{sel}");
        assert!(sel.contains("\"libfreemkv\": "), "{sel}");
    }

    // Every text rendering of `secret` a capture could leak: hex, base64 (padded
    // or not) and decimal byte arrays in Debug (`1, 2`) and JSON (`1,2`) spacing.
    fn encodings(secret: &[u8]) -> Vec<String> {
        let dec: Vec<String> = secret.iter().map(|b| b.to_string()).collect();
        let b64 = base64_encode(secret);
        vec![
            secret.iter().map(|b| format!("{b:02x}")).collect(),
            b64.trim_end_matches('=').to_string(),
            b64,
            dec.join(", "),
            dec.join(","),
        ]
    }

    // The fixture image scanned as a disc (its AACS state from the image's Unit_Key_RO.inf).
    fn scan(img: &libfreemkv::test_util::EncryptedBdImage) -> Disc {
        use libfreemkv::SectorSource;
        let mut src = img.source();
        let cap = src.capacity_sectors();
        Disc::scan_image(&mut src, cap, &libfreemkv::ScanOptions::default()).expect("scan")
    }

    // A keydb-like source that knows the rip's unit key: its answer needs no samples.
    struct KnownKey;

    impl libfreemkv::KeySource for KnownKey {
        fn get_unit_keys(
            &self,
            _: &dyn libfreemkv::keysource::ResolveCtx,
        ) -> libfreemkv::error::Result<Vec<libfreemkv::aacs::types::UnitKey>> {
            Ok(vec![libfreemkv::aacs::types::UnitKey::new(
                0,
                SECRET_UNIT_KEY,
            )])
        }
        fn answer_depends_on_samples(&self) -> bool {
            false
        }
    }

    // The rip's up-front key set over the fixture image (KU §3.3; the type lands at KU-L2).
    fn rip_key_set(
        img: &libfreemkv::test_util::EncryptedBdImage,
    ) -> libfreemkv::keys::ResolvedKeySet {
        let factory: libfreemkv::KeySourceFactory =
            std::sync::Arc::new(|| vec![Box::new(KnownKey) as Box<dyn libfreemkv::KeySource>]);
        libfreemkv::keys::ResolvedKeySet::resolve(
            &scan(img),
            &mut img.source(),
            libfreemkv::keys::KeyScope::WholeDisc,
            &factory,
            libfreemkv::keys::ResolveKeysOptions::default(),
        )
        .expect("the known key is proven on the stream")
        .keys
    }

    /// FK9 (KU design §3.3, §7.3): the `--share` capture of a disc whose stream is
    /// encrypted under the rip's key leaks no key, raw VID or MKB bytes into any file
    /// it writes, any console line, or the issue title and body.
    #[test]
    fn bug_report_capture_leaks_no_key_vid_or_mkb() {
        use libfreemkv::aacs::mkb::AacsVersion;
        use libfreemkv::test_util::{BdFile, decrypt_unit, encrypted_bd_image, unit_key_ro};

        let uk_ro = unit_key_ro(AacsVersion::V10, &[[0x42; 16]], &[1]);
        let img = encrypted_bd_image(
            &[
                BdFile::new("BDMV/PLAYLIST/00000.mpls", 3, None),
                BdFile::new("BDMV/CLIPINF/00001.clpi", 3, None),
                BdFile::new("BDMV/STREAM/00001.m2ts", 6, Some(SECRET_UNIT_KEY)),
            ],
            &uk_ro,
        );
        // The key is live: it opens the stream's first aligned unit (CPI masked).
        let at = img.files[2].0 as usize * 2048;
        let mut unit = img.image[at..at + 6144].to_vec();
        assert_ne!(unit, img.plain[at..at + 6144], "the stream is encrypted");
        decrypt_unit(&mut unit, &SECRET_UNIT_KEY);
        let mask = |u: &[u8]| -> Vec<u8> {
            let mut u = u.to_vec();
            u.chunks_mut(192).for_each(|p| p[0] &= 0x3F);
            u
        };
        assert_eq!(mask(&unit), mask(&img.plain[at..at + 6144]));

        // §3.3: the rip's key set over the same image, holding the known key and proven on
        // the stream, exists in memory while the capture runs (and the capture never sees it).
        let set = rip_key_set(&img);
        let status = set.status();
        assert_eq!((status.keyed, status.proven), (1, 1), "{status:?}");
        let scanned = scan(&img);
        let mut reader = set
            .whole_disc_reader(&scanned, img.source(), None)
            .expect("the set is for this image");
        let mut unit = vec![0u8; 6144];
        reader
            .read_sectors(img.files[2].0, 3, &mut unit, false)
            .expect("the set's key opens the stream");
        assert_eq!(mask(&unit), mask(&img.plain[at..at + 6144]));

        let disc = disc_with(Some(aacs_with_secrets(
            "0x1111111111111111111111111111111111111111",
        )));
        let dir = std::env::temp_dir().join(format!("fmkv-fk9-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A directory where the clip info would go: one skipped-file console line.
        std::fs::create_dir_all(dir.join("BDMV/CLIPINF/00001.clpi")).expect("blocker");
        let c = match capture(&disc, &mut img.source(), &dir, false) {
            Ok(c) => c,
            Err(lines) => panic!("capture failed: {lines:?}"),
        };
        assert_eq!(c.stderr.len(), 1, "{:?}", c.stderr);
        assert_eq!(c.stdout.len(), 2, "{:?}", c.stdout);

        let read = |n: &str| std::fs::read(dir.join(n)).expect("artifact");
        let text = |n: &str| String::from_utf8(read(n)).expect("utf-8");
        let mut texts: Vec<(String, String)> = vec![
            ("selection.json".into(), text("selection.json")),
            ("aacs.json".into(), text("aacs.json")),
            ("issue title".into(), c.title.clone()),
            ("issue body".into(), c.body.clone()),
        ];
        for (i, line) in c.stdout.iter().chain(&c.stderr).enumerate() {
            texts.push((format!("console line {i}"), line.clone()));
        }
        let structure = [("BDMV/PLAYLIST/00000.mpls", read("BDMV/PLAYLIST/00000.mpls"))];
        let _ = std::fs::remove_dir_all(&dir);

        let secrets: [(&str, &[u8]); 4] = [
            ("unit key", &SECRET_UNIT_KEY),
            ("raw Volume ID", &SECRET_VID),
            ("MKB bytes", &SECRET_MKB),
            ("uk_ro bytes", &[0x12, 0x34, 0x56]),
        ];
        for (what, secret) in secrets {
            for (name, t) in &texts {
                let lower = t.to_ascii_lowercase();
                // Hex in either case; base64 and decimal are case-exact.
                for (i, enc) in encodings(secret).into_iter().enumerate() {
                    let hit = if i == 0 {
                        lower.contains(&enc)
                    } else {
                        t.contains(&enc)
                    };
                    assert!(!hit, "{what} leaked into {name} as {enc:?}");
                }
            }
            for (name, data) in &structure {
                let hit = data.windows(secret.len()).any(|w| w == secret);
                assert!(!hit, "{what} bytes leaked into {name}");
            }
        }
    }

    /// FK9 structural half: the capture API is handed only a `Disc` and a raw reader,
    /// never the rip's key set (KU design §3.3).
    #[test]
    fn the_capture_api_takes_no_key_set() {
        let src = include_str!("disc_capture.rs");
        let api = &src[..src.find("#[cfg(test)]").expect("test module")];
        for banned in ["ResolvedKeySet", "KeyFetch", "DecryptKeys"] {
            assert!(!api.contains(banned), "disc_capture API names {banned}");
        }
    }
}

#[cfg(test)]
mod fold_structure_tests {
    use super::fold_structure;
    use libfreemkv::SectorSource;

    struct Blank;
    impl SectorSource for Blank {
        fn capacity_sectors(&self) -> u32 {
            1024
        }
        fn read_sectors(
            &mut self,
            _lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::error::Result<usize> {
            let n = count as usize * 2048;
            buf[..n].fill(0);
            Ok(n)
        }
    }

    // A read that cannot find a filesystem must surface its cause, not
    // collapse into the same `None` as an empty-but-valid tree.
    #[test]
    fn an_unreadable_structure_reports_the_error_and_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("fmkv-fold-{}", std::process::id()));
        let mut written = Vec::new();
        let r = fold_structure(&dir, &mut written, &mut Blank);
        assert!(
            r.is_err(),
            "expected the read error, got {:?}",
            r.map(|s| s.is_some())
        );
        assert!(written.is_empty());
    }
}

#[cfg(test)]
mod share_fix_tests {
    use super::*;
    use libfreemkv::disc::DiscRegion;
    use libfreemkv::{DiscFormat, Error};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmkv-share-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn put(root: &std::path::Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, bytes).expect("write fixture");
    }

    fn bare_disc(encrypted: bool, aacs_error: Option<Error>) -> Disc {
        Disc {
            volume_id: "T".to_string(),
            meta_title: None,
            format: DiscFormat::BluRay,
            capacity_sectors: 0,
            capacity_bytes: 0,
            layers: 1,
            titles: Vec::new(),
            region: DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted,
            aacs_error,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    fn summary() -> DiscSummary {
        DiscSummary {
            file_count: 1,
            total_bytes: 1,
            skipped: Vec::new(),
        }
    }

    // K4: a FAILED AACS step must say so (with its code), not blame a keyless scan.
    #[test]
    fn a_failed_aacs_step_is_reported_as_a_failure() {
        let disc = bare_disc(true, Some(Error::AacsNoKeys));
        let json = aacs_json(&disc);
        let code = format!("E{}", Error::AacsNoKeys.code());
        assert!(
            json.contains(&format!("\"aacs_error\": \"{code}\"")),
            "{json}"
        );
        assert!(!json.contains("keyless") && !json.contains("-v"), "{json}");
        let body = issue_body(&disc, &summary(), "QUJD").0;
        assert!(!body.contains("keyless") && body.contains(&code), "{body}");
    }

    // K4: no AACS on the disc (DVD / clear BD) is not a missing capture.
    #[test]
    fn a_disc_without_aacs_says_so() {
        let disc = bare_disc(false, None);
        let json = aacs_json(&disc);
        assert!(!json.contains("keyless") && !json.contains("-v"), "{json}");
        assert!(json.contains("\"aacs_present\": false"), "{json}");
        let body = issue_body(&disc, &summary(), "QUJD").0;
        assert!(!body.contains("keyless"), "{body}");
    }

    // L7: a real BD's zip base64 blows GitHub's 65536-char body cap; point at the file instead.
    #[test]
    fn an_oversized_zip_is_not_inlined() {
        let disc = bare_disc(false, None);
        let big = "A".repeat(200_000);
        let body = issue_body(&disc, &summary(), &big).0;
        assert!(
            body.chars().count() <= crate::info::BODY_INLINE_BUDGET_CHARS,
            "body is {} chars",
            body.chars().count()
        );
        assert!(body.contains("profile.zip"), "{body}");
        let small = issue_body(&disc, &summary(), "QUJD").0;
        assert!(
            small.contains("QUJD"),
            "a small zip is still inlined: {small}"
        );
    }

    fn bd_folder(tag: &str) -> PathBuf {
        let src = scratch(tag).join("disc");
        put(&src, "BDMV/index.bdmv", b"INDX0200");
        put(&src, "BDMV/PLAYLIST/00001.mpls", b"MPLS0200-playlist");
        put(&src, "BDMV/CLIPINF/00001.clpi", b"HDMV0200-clip");
        std::fs::create_dir_all(src.join("BDMV/STREAM")).expect("stream dir");
        src
    }

    // L1: only plain `/`-separated components survive, on every OS.
    #[test]
    fn only_plain_portable_paths_are_safe() {
        for ok in [
            "BDMV/PLAYLIST/00001.mpls",
            "VIDEO_TS/VTS_01_0.IFO",
            "BDMV/META/DL/bdmt_eng.xml",
        ] {
            assert!(is_safe_rel_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/x",
            "BDMV//x",
            "BDMV/../x",
            "..",
            "./x",
            "a\\..\\x.xml",
            "C:x.xml",
            "BDMV/a:b.xml",
            "BDMV/a*.xml",
            "BDMV/x.xml.",
            "BDMV/x.xml ",
            "BDMV/CON.xml",
            "BDMV/com1.mpls",
            "BDMV/CON .xml",
            "BDMV/CONIN$",
            "BDMV/conout$.txt",
            "BDMV/COM\u{B9}.mpls",
            "BDMV/LPT\u{B3}",
            "BDMV/COM0",
            "BDMV/LPT9",
            "BDMV/a\u{1}.xml",
            "BDMV/a\".xml",
            "BDMV/a|b",
        ] {
            assert!(!is_safe_rel_path(bad), "{bad:?} accepted");
        }
        assert!(
            is_safe_rel_path("BDMV/CONSOLE.xml"),
            "only exact DOS device stems are reserved"
        );
    }

    // T11: the success path writes nested names and zips exactly those entries.
    #[test]
    fn a_structure_fold_writes_nested_files_and_zips_them() {
        let src = bd_folder("ok");
        let out = src.parent().expect("parent").join("profile");
        std::fs::create_dir_all(&out).expect("profile dir");
        let mut img = libfreemkv::DirImage::open(&src).expect("dir image");
        let mut written = Vec::new();
        let s = fold_structure(&out, &mut written, &mut img)
            .expect("fold ok")
            .expect("files found");
        for rel in ["BDMV/PLAYLIST/00001.mpls", "BDMV/CLIPINF/00001.clpi"] {
            assert!(written.iter().any(|w| w == rel), "{rel} not in {written:?}");
            assert!(out.join(rel).is_file(), "{rel} not written");
        }
        assert_eq!(s.file_count, written.len());
        let zip = crate::info::zip_files(&out, &written).expect("zip");
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(zip)).expect("valid zip");
        let names: Vec<String> = (0..a.len())
            .map(|i| a.by_index(i).expect("entry").name().to_string())
            .collect();
        assert_eq!(names, written);
    }

    // L1: a disc-supplied name carrying a separator or `..` never reaches the filesystem.
    #[cfg(unix)]
    #[test]
    fn a_hostile_disc_name_is_skipped_not_joined() {
        let src = bd_folder("evil");
        put(&src, "BDMV/META/DL/x\\..\\..\\..\\..\\evil.xml", b"<x/>");
        put(&src, "BDMV/META/DL/ok.xml", b"<x/>");
        let out = src.parent().expect("parent").join("profile");
        std::fs::create_dir_all(&out).expect("profile dir");
        let mut img = libfreemkv::DirImage::open(&src).expect("dir image");
        let mut written = Vec::new();
        let _ = fold_structure(&out, &mut written, &mut img).expect("fold ok");
        assert!(
            written.iter().any(|w| w == "BDMV/META/DL/ok.xml"),
            "{written:?}"
        );
        assert!(
            !written.iter().any(|w| w.contains('\\') || w.contains("..")),
            "a hostile name was written: {written:?}"
        );
    }

    // Every file refused: the fold still returns (count 0 + reasons) so callers can say so.
    #[test]
    fn a_fold_where_every_write_fails_reports_zero_saved() {
        let src = bd_folder("allfail");
        let out = src.parent().expect("parent").join("profile");
        std::fs::write(&out, b"not a dir").expect("blocker file");
        let mut img = libfreemkv::DirImage::open(&src).expect("dir image");
        let mut written = Vec::new();
        let s = fold_structure(&out, &mut written, &mut img)
            .expect("fold ok")
            .expect("files found");
        assert_eq!(s.file_count, 0);
        assert!(written.is_empty() && !s.skipped.is_empty());
        assert!(all_skipped_line(&s).contains(&s.skipped.len().to_string()));
    }

    // L14: one unwritable file must not end the process; the rest still land.
    #[test]
    fn one_failed_write_skips_that_file_only() {
        let src = bd_folder("fail");
        let out = src.parent().expect("parent").join("profile");
        std::fs::create_dir_all(out.join("BDMV/PLAYLIST/00001.mpls")).expect("blocker");
        let mut img = libfreemkv::DirImage::open(&src).expect("dir image");
        let mut written = Vec::new();
        let _ = fold_structure(&out, &mut written, &mut img).expect("fold ok");
        assert!(
            written.iter().any(|w| w == "BDMV/CLIPINF/00001.clpi"),
            "{written:?}"
        );
        assert!(
            !written.iter().any(|w| w == "BDMV/PLAYLIST/00001.mpls"),
            "{written:?}"
        );
    }
}
