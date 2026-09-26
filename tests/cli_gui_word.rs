//! `freemkv gui` in the CLI build (no `gui` feature) points at the app download
//! instead of misreading "gui" as a URL.
#![cfg(not(feature = "gui"))]

use std::process::Command;

#[test]
fn gui_word_in_cli_build_points_to_the_app_download() {
    let out = Command::new(env!("CARGO_BIN_EXE_freemkv"))
        .args(["gui", "--language", "en"])
        .output()
        .expect("spawn freemkv");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("https://freemkv.org/download"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("not a valid URL"), "stderr: {stderr}");
}
