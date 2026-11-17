//! `capture` over a scanned BD image: a real title list, its selection JSON and the issue body.
use super::{capture, selection_json};
use crate::ku_fixtures::{K1, bd_image};

#[test]
fn a_scanned_disc_is_captured_with_its_titles_and_selection() {
    for (clips, tag) in [(vec![None, None], "clear"), (vec![Some(K1), None], "enc")] {
        let fx = bd_image(&clips, 1);
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(format!("profile-{tag}"));
        let mut src = fx.source();
        let c = capture(&fx.disc, &mut src, &dir, false)
            .unwrap_or_else(|e| panic!("{tag}: capture failed: {e:?}"));
        assert!(c.zip_path.is_file(), "{tag}: no zip");
        assert!(c.body.contains("Selected title:"), "{tag}: {}", c.body);
        assert!(c.body.contains("## AACS diagnostics"), "{tag}: {}", c.body);
        let json = selection_json(&fx.disc);
        assert!(json.contains("\"playlist_id\""), "{json}");
        assert!(
            json.contains(&format!("\"title_count\": {}", fx.disc.titles.len())),
            "{json}"
        );
        // Quiet still captures; it only drops the console lines.
        let mut src = fx.source();
        let quiet = capture(&fx.disc, &mut src, &tmp.path().join("q"), true);
        assert!(quiet.is_ok(), "{tag}");
    }
}

#[test]
fn a_profile_directory_that_cannot_be_created_is_a_hard_failure() {
    let fx = bd_image(&[None], 1);
    let tmp = tempfile::tempdir().unwrap();
    let blocker = tmp.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    let mut src = fx.source();
    let Err(lines) = capture(&fx.disc, &mut src, &blocker.join("sub"), false) else {
        panic!("a directory under a regular file was created");
    };
    assert_eq!(lines.len(), 1, "{lines:?}");
}
