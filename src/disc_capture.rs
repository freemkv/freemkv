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
//! On a keyless scan (no AACS handshake) the disc may carry no AACS state, so
//! the profile records a clear "not captured — run with -v" marker instead.

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

// Minimal JSON string escaper for the machine-artifact profiles below.
fn json_esc(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect()
}

/// Non-secret AACS diagnostics distilled from a scanned disc, for keydb / AACS
/// resolution triage (issue #46). Deliberately omits EVERY secret: no VUK, no
/// unit keys, no raw Volume ID, no MKB / Unit_Key_RO bytes — only crypto *shape*
/// plus the disc hash, which is the public keydb lookup key (SHA-1 of
/// `Unit_Key_RO.inf`, the same value a maintainer matches against keydb rows).
pub(crate) struct AacsDiag {
    // Whether the scan carried any AACS state at all. A plain (keyless) ISO /
    // folder scan may leave `disc.aacs == None`, so this is `false` and the
    // profile records the "run with -v" marker instead of crypto shape.
    pub captured: bool,
    // 40-hex keydb lookup key (no `0x` prefix), when the scan read the disc's
    // `Unit_Key_RO.inf`. `None` when uncaptured or the hash was blank.
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
            // `disc_hash` is stored with an `0x` prefix; keydb rows are keyed by
            // the bare 40-hex, so strip it to the comparable lookup form.
            let hash = libfreemkv::hex::strip_hex_prefix(a.disc_hash.trim()).trim();
            AacsDiag {
                captured: true,
                disc_hash: (!hash.is_empty()).then(|| hash.to_string()),
                generation: Some(a.version),
                mkb_version: a.mkb_version,
                bus_encryption: Some(a.bus_encryption),
                // Raw VID stays private — report only whether one was obtained.
                vid_available: a.volume_id.iter().any(|&b| b != 0),
            }
        }
        None => AacsDiag {
            captured: false,
            disc_hash: None,
            generation: None,
            mkb_version: None,
            bus_encryption: None,
            vid_available: false,
        },
    }
}

// Guidance shown when a keyless scan left no AACS data — tells a reporter how
// to include the disc hash next time. Shared by the profile note + on-run line.
const AACS_ABSENT_HINT: &str = "AACS diagnostics not captured — this was a keyless scan (no AACS handshake). \
     Re-run `freemkv info <disc> -v --share` (verbose runs the handshake) to include \
     disc_hash, the keydb lookup key, for keydb/AACS triage.";

/// The `aacs.json` profile member: non-secret AACS diagnostics for keydb triage.
/// When the scan carried no AACS state, records a clear "not captured" marker
/// with the `-v` hint. NEVER emits key material, VUK, unit keys, MKB bytes, or
/// the raw Volume ID — only shape + the public disc hash. Literal English/JSON.
pub(crate) fn aacs_json(disc: &Disc) -> String {
    let d = aacs_diag(disc);
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!("  \"aacs_captured\": {},\n", d.captured));
    if d.captured {
        match &d.disc_hash {
            Some(h) => s.push_str(&format!("  \"disc_hash\": \"{}\",\n", json_esc(h))),
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
            "  \"note\": \"disc_hash is the AACS keydb lookup key (SHA-1 of Unit_Key_RO.inf); \
             compare it against keydb rows. Present only when the scan read the disc's AACS data. \
             No key material, VUK, unit keys, MKB bytes, or raw Volume ID is included — \
             vid_available only reports whether the SCSI handshake yielded a Volume ID.\"\n",
        );
    } else {
        s.push_str(&format!("  \"note\": \"{}\"\n", json_esc(AACS_ABSENT_HINT)));
    }
    s.push_str("}\n");
    s
}

// A JSON view of freemkv's decision so a maintainer sees the selection without
// re-running: picked title first (`titles[0]`), then every title's shape.
// Literal English/JSON — a machine artifact, not localized UI.
pub(crate) fn selection_json(disc: &Disc) -> String {
    let esc = json_esc;
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

    // Non-secret AACS diagnostics for keydb / AACS resolution triage (issue #46):
    // the computed disc hash (keydb lookup key) + crypto shape, or a "run with
    // -v" marker on a keyless scan. Never carries key material — see `aacs_json`.
    save_bin(
        &profile_dir,
        "aacs.json",
        aacs_json(disc).as_bytes(),
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

    // AACS triage guidance (issue #46): confirm the disc hash was bundled, or —
    // on a keyless scan that yielded no AACS state — tell the reporter to re-run
    // with `-v` so the keydb lookup key ends up in the profile next time.
    let diag = aacs_diag(disc);
    match diag.disc_hash {
        Some(hash) => println!(
            "{}",
            strings::fmt_or(
                "disc.capture_aacs_hash",
                "AACS diagnostics: disc hash {hash} (keydb lookup key) recorded in aacs.json.",
                &[("hash", &hash)],
            )
        ),
        None => println!(
            "{}",
            strings::get_or("disc.capture_aacs_absent", AACS_ABSENT_HINT)
        ),
    }

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

    // AACS diagnostics (issue #46): the disc hash (keydb lookup key) + crypto
    // shape a maintainer needs to compare against keydb rows — or a "run with
    // -v" marker when this was a keyless scan. Non-secret only; see `aacs_json`.
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
        body.push_str("Not captured — keyless scan (no AACS handshake).\n");
    }
    body.push_str("```\n\n");
    if !diag.captured {
        body.push_str(AACS_ABSENT_HINT);
        body.push_str("\n\n");
    }

    body.push_str(
        "Metadata only — no audio/video essence, no AACS keys (no VUK, unit keys, MKB \
         bytes, or raw Volume ID). `selection.json` in the zip shows freemkv's full \
         title ranking; `aacs.json` carries the non-secret AACS diagnostics above \
         (disc hash, version, vid-available) for keydb triage.\n\n",
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

#[cfg(test)]
mod aacs_diag_tests {
    use super::*;
    use libfreemkv::disc::DiscRegion;
    use libfreemkv::{AacsState, Disc, DiscFormat, KeyOrigin};

    // Distinctive secret byte fills — if any leak into a profile artifact the
    // no-key-material assertions below will catch their hex.
    const SECRET_VUK: [u8; 16] = [0xAB; 16];
    const SECRET_UNIT_KEY: [u8; 16] = [0xCD; 16];
    const SECRET_VID: [u8; 16] = [0xEF; 16];

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

    // An AACS state carrying a real disc hash AND every secret field populated,
    // so profile output can be checked to leak the hash but none of the secrets.
    fn aacs_with_secrets(disc_hash: &str) -> AacsState {
        AacsState {
            version: 2,
            bus_encryption: true,
            mkb_version: Some(77),
            disc_hash: disc_hash.to_string(),
            key_source: KeyOrigin::ExternalUk,
            vuk: Some(SECRET_VUK),
            unit_keys: vec![(0, SECRET_UNIT_KEY)],
            volume_id: SECRET_VID,
            uk_ro: vec![0x12, 0x34, 0x56],
            mkb: vec![0x78, 0x9a],
        }
    }

    #[test]
    fn aacs_present_puts_disc_hash_into_profile() {
        // Stored with the `0x` prefix; the profile must record the bare 40-hex
        // keydb lookup key.
        let raw = "0xaabbccddeeff00112233445566778899aabbccdd";
        let bare = "aabbccddeeff00112233445566778899aabbccdd";
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
            },
            "QUJD",
        );
        assert!(body.contains("## AACS diagnostics"), "{body}");
        assert!(
            body.contains(bare),
            "issue body must name the disc hash: {body}"
        );
    }

    #[test]
    fn keyless_disc_records_not_captured_marker() {
        let disc = disc_with(None);
        let json = aacs_json(&disc);
        assert!(json.contains("\"aacs_captured\": false"), "{json}");
        assert!(json.contains("-v"), "hint must point at verbose: {json}");
        // No crypto-shape fields when nothing was captured (the note prose may
        // still mention disc_hash — assert on the JSON key form only).
        assert!(!json.contains("\"disc_hash\""), "{json}");
        assert!(!json.contains("\"vid_available\""), "{json}");

        let body = issue_body(
            &disc,
            &DiscSummary {
                file_count: 1,
                total_bytes: 10,
            },
            "QUJD",
        );
        assert!(
            body.contains("Not captured — keyless scan"),
            "issue body must flag the keyless case: {body}"
        );
    }

    #[test]
    fn no_key_material_is_ever_emitted() {
        let raw = "0x1111111111111111111111111111111111111111";
        let disc = disc_with(Some(aacs_with_secrets(raw)));

        // Every artifact a reporter could paste to a public issue.
        let json = aacs_json(&disc);
        let sel = selection_json(&disc);
        let body = issue_body(
            &disc,
            &DiscSummary {
                file_count: 1,
                total_bytes: 1,
            },
            "QUJD",
        );

        // Hex of each secret fill and the raw uk_ro / mkb bytes.
        let leaks = [
            "abababab", // VUK
            "cdcdcdcd", // unit key
            "efefefef", // raw Volume ID
            "123456",   // uk_ro bytes
            "789a",     // mkb bytes
        ];
        for artifact in [&json, &sel, &body] {
            let lower = artifact.to_ascii_lowercase();
            for needle in leaks {
                assert!(
                    !lower.contains(needle),
                    "secret material {needle:?} leaked into a shared artifact:\n{artifact}"
                );
            }
        }
    }
}
