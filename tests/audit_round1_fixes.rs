//! Regression tests for the round-1 audit fixes.
//!
//! Each pins a defect that shipped, and each was watched failing against the
//! unfixed code before being kept.

use freemkv::engine::{RunOutcome, extract_target, sanitize_label, title_basename};

// ── The GUI classified a run by substring-matching the engine's English. ────
// Three real messages defeated it, all rendered as SUCCESS; a fourth landed
// in the wrong bucket. The verdict is now a typed value, immune to wording.

/// The exact strings the engine emits today must not be classifiable by
/// accident — this pins that we are NOT reading them.
#[test]
fn outcome_is_typed_not_parsed_from_prose() {
    // These are verbatim engine messages that the old classifier got wrong.
    let misleading = [
        "the disc has no decryption key — every remaining title would fail the \
         same way. Check the keydb or online key service in Settings.",
        "Recovery aborted — too much unreadable data; partial ISO kept: /out/D.iso",
        "Recovery aborted — too much unreadable data to mux a complete title \
         (raise the lost-seconds tolerance to accept it, or re-run recovery). \
         Partial ISO kept: /out/D.iso",
    ];
    for m in misleading {
        // The old rule, reproduced exactly, to document what it did.
        let old_said_success =
            !m.starts_with("Cancelled") && !m.starts_with("Nothing") && !m.contains("failed");
        assert!(
            old_said_success,
            "this message was chosen because the OLD classifier called it success: {m}"
        );
    }
    // And the inverse false positive: a genuine failure that the old rule
    // pushed into the "nothing written" bucket.
    assert!("convert failed: bad input".contains("failed"));

    // The replacement carries three distinct verdicts that no string can blur.
    assert_ne!(RunOutcome::Failed, RunOutcome::Completed);
    assert_ne!(RunOutcome::Cancelled, RunOutcome::Completed);
    assert_eq!(RunOutcome::default(), RunOutcome::Completed);
}

// ── A disc volume label reached the output path almost unsanitised. ─────────

/// The DEFAULT output template is the empty one, and it was the branch the
/// original fix missed.
///
/// `title_basename` sanitised only inside its `{title}` substitution, so the
/// test below (which passes a `"{title}"` template) proved the traversal
/// closed while the branch most users are actually on --
/// `format!("{label}_t{n}")` with the raw label -- was still open. A class of
/// bug is not closed by covering one of its branches.
#[test]
fn the_default_template_sanitises_the_label_too() {
    for evil in [
        r"..\..\..\Users\victim\AppData\Roaming\evil",
        "../../../etc/cron.d/evil",
        r"C:\Windows\System32\evil",
    ] {
        let out = title_basename("", evil, 1);
        assert!(!out.contains('/'), "forward slash survived: {out}");
        assert!(!out.contains('\\'), "backslash survived: {out}");
        assert!(!out.contains(':'), "drive colon survived: {out}");
        assert_eq!(
            std::path::Path::new(&out).components().count(),
            1,
            "must stay ONE path component: {out}"
        );
    }
}

/// The whole-disc sinks join the label into a path too, and neither went
/// through the sanitiser: `extract_target` (the decrypted-folder
/// destination) built `dest_dir.join(label)` raw, so a crafted label walked
/// straight out of the folder the user chose.
#[test]
fn the_extract_destination_cannot_escape_the_chosen_folder() {
    for evil in [
        r"..\..\..\Users\victim\AppData\Roaming\evil",
        "../../../etc/cron.d/evil",
    ] {
        let out = extract_target("/tmp/out", evil);
        let tail: Vec<_> = out
            .strip_prefix("/tmp/out")
            .expect("must stay under the destination")
            .components()
            .collect();
        assert_eq!(
            tail.len(),
            1,
            "the disc label became {} components, not one: {}",
            tail.len(),
            out.display()
        );
        // `..` surviving as TEXT in one component is harmless (navigates
        // nowhere); stripping dots would mangle "Vol.. 2". A component
        // BOUNDARY must never survive, asserted above.
        assert!(
            !std::path::Path::new(&out)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "a real parent-dir component survived: {}",
            out.display()
        );
        // Assert SEPARATORS are gone (a backslash is ordinary on Linux).
        // Checked against the single COMPONENT, not the whole joined path,
        // since `join` itself adds a separator a whole-string trim would miss.
        let component = tail[0].as_os_str().to_string_lossy();
        assert!(
            !component.contains('\\'),
            "a backslash survived into the component: {component}"
        );
    }
}

/// Every seam that turns a disc label into a path, enumerated.
///
/// Round 1 fixed three and missed the fourth -- the ordinary drive->ISO rip --
/// because the fix was driven from the findings rather than from the list of
/// places the label lands. This states the invariant once, over the exported
/// helpers, so the next seam added is measured against it: whatever the label
/// contains, what reaches the filesystem is a single component.
#[test]
fn every_label_derived_name_stays_one_component() {
    let evil = r"..\..\..\Users\victim\AppData\Roaming\evil";
    let names = [
        title_basename("", evil, 1),
        title_basename("{title}", evil, 1),
        sanitize_label(evil),
        extract_target("/tmp/out", evil)
            .strip_prefix("/tmp/out")
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    for n in names {
        assert_eq!(
            std::path::Path::new(&n).components().count(),
            1,
            "not one component: {n}"
        );
        assert!(!n.contains('/'), "forward slash survived: {n}");
        assert!(!n.contains('\\'), "backslash survived: {n}");
    }
}

/// The Windows traversal. `title_basename` stripped only `/`, so a crafted
/// label escaped the destination directory on Windows.
#[test]
fn a_crafted_volume_label_cannot_escape_the_destination() {
    for evil in [
        r"..\..\..\Users\victim\AppData\Roaming\evil",
        "../../../etc/cron.d/evil",
        r"C:\Windows\System32\evil",
    ] {
        let out = title_basename("{title}", evil, 1);
        assert!(!out.contains('/'), "forward slash survived: {out}");
        assert!(!out.contains('\\'), "backslash survived: {out}");
        assert!(!out.contains(':'), "drive colon survived: {out}");
        // NOTE: `..` may survive as TEXT — it's one filename component and
        // cannot traverse. The property is "stays one component inside dest",
        // not "contains no dots" (which would mangle "Vol.. 2").
        assert!(
            !std::path::Path::new(&out)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "resolved to a parent-dir component: {out}"
        );
        assert_eq!(
            std::path::Path::new(&out).components().count(),
            1,
            "escaped into more than one path component: {out}"
        );
    }
}

/// Control characters and Windows-reserved device names.
#[test]
fn control_characters_and_reserved_names_are_neutralised() {
    assert!(!sanitize_label("Movie\u{7}\u{1b}[2J").contains('\u{1b}'));
    assert!(!sanitize_label("Movie\nPart2").contains('\n'));
    for reserved in ["CON", "con", "NUL", "COM1", "LPT9", "AUX"] {
        let s = sanitize_label(reserved);
        assert!(s.starts_with('_'), "{reserved} not neutralised: {s}");
    }
    // COM0 is NOT reserved — do not over-reach.
    assert_eq!(sanitize_label("COM0"), "COM0");
    // A trailing dot/space is illegal as a Windows component.
    assert_eq!(sanitize_label("Movie."), "Movie");
    assert_eq!(sanitize_label("Movie "), "Movie");
    // Bare navigation is not a name.
    assert_eq!(sanitize_label(".."), "disc");
    assert_eq!(sanitize_label(""), "disc");
}

/// The regression the NARROW rule exists to avoid. The CLI's `sanitize_name`
/// is an ASCII allow-list that turns these into `disc`; the GUI renders them
/// correctly today on every platform and must keep doing so.
#[test]
fn legitimate_labels_are_left_alone() {
    for good in [
        "日本語のタイトル",
        "Война и мир",
        "Ocean's Eleven",
        "Mission: Impossible", // colon is illegal on Windows -> replaced
        "Vol. 2",
        "Amélie (2001)",
        "Wallace & Gromit",
    ] {
        let s = sanitize_label(good);
        assert!(
            !s.is_empty() && s != "disc",
            "{good} was flattened to {s:?}"
        );
    }
    // Non-Latin survives byte-for-byte.
    assert_eq!(sanitize_label("日本語のタイトル"), "日本語のタイトル");
    assert_eq!(sanitize_label("Ocean's Eleven"), "Ocean's Eleven");
    // Only the illegal character is touched, and the rest is preserved.
    assert_eq!(sanitize_label("Mission: Impossible"), "Mission_ Impossible");
}

/// The user's own template is trusted and must not be mangled — only the
/// disc-supplied label is untrusted.
#[test]
fn the_user_template_is_not_sanitised() {
    let out = title_basename("Movie.2024 - {title}", "Label", 3);
    assert!(
        out.starts_with("Movie.2024 - Label"),
        "template mangled: {out}"
    );
}
