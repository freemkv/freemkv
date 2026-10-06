use super::{consent_granted, may_prompt_for_consent, zip_files};

// Submitting requires EXPLICIT consent with the locale's accept token, never a hard-coded
// ASCII "y" — a crafted `de.json` can render "[j/N]" (default NO), so a bare Enter must not
// post.
#[test]
fn submitting_requires_an_explicit_locale_matched_yes() {
    // A bare Enter is NOT consent — it declines, whatever the prompt hinted.
    assert!(!consent_granted(1, "", "y"), "a bare Enter must not post");
    assert!(!consent_granted(1, "", "j"), "a bare Enter must not post");
    // EOF (n == 0) is never consent, even if the buffer somehow looks right.
    assert!(!consent_granted(0, "y", "y"), "EOF is never consent");
    // The affirmative token is the locale's, matched case-insensitively.
    assert!(consent_granted(1, "y", "y"));
    assert!(consent_granted(1, "Y", "y"));
    assert!(
        consent_granted(1, "j", "j"),
        "German 'j' posts under a de prompt"
    );
    assert!(consent_granted(1, "J", "j"));
    // A German 'j' must NOT post while the ASCII 'y' token is active, and an
    // English 'y' must not post under a German 'j' prompt — the token and
    // the prompt are the same locale, so a mismatch fails closed.
    assert!(!consent_granted(1, "j", "y"));
    assert!(!consent_granted(1, "y", "j"));
    // Anything else declines.
    assert!(!consent_granted(1, "n", "y"));
    assert!(!consent_granted(1, "yes please", "y"));
}
use std::io::Read as _;

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// A scratch directory unique to this process and call — a fixed name is
// shared state, and two `cargo test` processes deleting each other's
// fixtures mid-assertion reads as a (false) share-safety failure.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("fmkv-{}-{}-{}", tag, std::process::id(), n))
}

fn entries(zip: &[u8]) -> Vec<String> {
    let mut a = zip::ZipArchive::new(std::io::Cursor::new(zip.to_vec())).expect("valid zip");
    (0..a.len())
        .map(|i| a.by_index(i).unwrap().name().to_string())
        .collect()
}

// The archive is bounded by what this run WROTE, not what happens to be in the directory
// (which could include a previous UNMASKED run's capture, or unrelated local files).
#[test]
fn the_archive_carries_only_the_files_this_run_wrote() {
    let dir = scratch_dir("zip-manifest-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(dir.join("inquiry.bin"), b"this run").unwrap();
    std::fs::write(dir.join("drive.toml"), b"[drive]\n").unwrap();
    // Left over from an earlier, UNMASKED run of a different drive.
    std::fs::write(dir.join("gc_0108.bin"), b"stale serial").unwrap();
    // Nothing to do with freemkv at all.
    std::fs::write(dir.join("tax-return.pdf"), b"private").unwrap();

    let got = entries(&zip_files(&dir, &names(&["inquiry.bin", "drive.toml"])).expect("zip built"));
    assert_eq!(got, vec!["inquiry.bin", "drive.toml"]);
    assert!(
        !got.iter().any(|n| n == "gc_0108.bin"),
        "a previous run's unmasked capture was published: {got:?}"
    );
    assert!(
        !got.iter().any(|n| n == "tax-return.pdf"),
        "an unrelated local file was published: {got:?}"
    );

    // The contents really are the named files, not just the names.
    let mut a = zip::ZipArchive::new(std::io::Cursor::new(
        zip_files(&dir, &names(&["inquiry.bin"])).unwrap(),
    ))
    .unwrap();
    let mut body = String::new();
    a.by_name("inquiry.bin")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "this run");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A manifest entry that is not on disk, a duplicate, and our own output
/// are all handled without failing the submission or corrupting the zip.
#[test]
fn a_missing_duplicate_or_self_referential_entry_does_not_break_the_archive() {
    let dir = scratch_dir("zip-edge-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("inquiry.bin"), b"x").unwrap();
    std::fs::write(dir.join("profile.zip"), b"previous archive").unwrap();

    let got = entries(
        &zip_files(
            &dir,
            &names(&[
                "inquiry.bin",
                "inquiry.bin",
                "never_written.bin",
                "profile.zip",
            ]),
        )
        .expect("a missing entry must not fail the submission"),
    );
    assert_eq!(
        got,
        vec!["inquiry.bin"],
        "the archive must not nest itself or repeat an entry"
    );

    // An empty manifest is a valid, empty archive rather than an error.
    assert!(entries(&zip_files(&dir, &[]).expect("empty manifest is valid")).is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

// The consent prompt is only offered when the user can actually READ it: testing stdin
// alone let `--share 2>/dev/null` block on a read with nothing on screen, and bare Enter is
// the [Y] default.
#[test]
fn consent_is_only_offered_when_the_question_is_visible_and_answerable() {
    // Both channels interactive: ask.
    assert!(may_prompt_for_consent("tok", true, true));

    // stderr redirected — the question would be invisible.
    assert!(!may_prompt_for_consent("tok", true, false));
    // stdin redirected/piped — no informed answer is possible.
    assert!(!may_prompt_for_consent("tok", false, true));
    assert!(!may_prompt_for_consent("tok", false, false));

    // No compiled-in token: nothing to submit with, so never ask. (The
    // caller trims before passing, so the empty case covers whitespace.)
    assert!(!may_prompt_for_consent("", true, true));
}
