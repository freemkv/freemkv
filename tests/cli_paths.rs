//! CLI dispatch paths run end to end: diagnostic logging, help routes, update-keys and
//! info on stream containers. No hardware, no network beyond a closed loopback port.

use std::process::{Command, Output};

fn freemkv() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_freemkv"));
    c.env("LC_ALL", "en_US.UTF-8")
        .env_remove("LANGUAGE")
        .env_remove("RUST_LOG");
    c
}

fn run(args: &[&str]) -> Output {
    freemkv().args(args).output().expect("failed to run")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d =
            std::env::temp_dir().join(format!("freemkv_cli_paths_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        Scratch(d)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_requested_log_file_is_written_at_every_level() {
    let s = Scratch::new("log");
    for level in ["1", "2", "3", "4"] {
        let log = s.path(&format!("diag{level}.log"));
        let out = run(&["version", "--log-level", level, "--log-file", &log]);
        assert!(out.status.success(), "{}", text(&out));
        assert!(std::path::Path::new(&log).exists(), "level {level}: no log");
    }
    // A log file alone defaults to debug.
    let log = s.path("only-file.log");
    let out = run(&["version", "--log-file", &log]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(std::path::Path::new(&log).exists());
}

#[test]
fn an_unopenable_log_file_is_reported_and_the_command_still_runs() {
    let s = Scratch::new("badlog");
    std::fs::write(s.0.join("plain"), b"x").unwrap();
    let bad = s.path("plain/sub/diag.log");
    let out = run(&["version", "--log-file", &bad]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains(env!("CARGO_PKG_VERSION")), "{t}");
    assert!(t.contains("plain"), "the bad path must be named: {t}");
}

#[test]
fn log_flag_mistakes_are_reported_once_the_locale_is_known() {
    for args in [
        &["version", "--log-level", "0"][..],
        &["version", "--log-level", "loud"][..],
        &["version", "--log-level"][..],
        &["version", "--log-file"][..],
    ] {
        let out = run(args);
        let t = text(&out);
        assert!(out.status.success(), "{args:?}: {t}");
        assert!(t.contains("--log-"), "{args:?}: no diagnostic: {t}");
    }
}

#[test]
fn every_help_route_prints_and_an_unknown_topic_fails() {
    for args in [
        &["help", "info"][..],
        &["help", "update-keys"][..],
        &["help", "version"][..],
        &["info", "--help"][..],
        &["update-keys", "--help"][..],
        &["--version"][..],
    ] {
        let out = run(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        assert!(!out.stdout.is_empty(), "{args:?}: no output");
    }
    let out = run(&["help", "no-such-topic"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("no-such-topic"));
}

#[test]
fn update_keys_needs_a_url_and_reports_a_failed_fetch() {
    let out = run(&["update-keys"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let s = Scratch::new("keys");
    let dest = s.path("keydb.cfg");
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    for url in [
        "ftp://example.invalid/keydb.cfg".to_string(),
        format!("http://127.0.0.1:{port}/keydb.cfg"),
    ] {
        let out = run(&["update-keys", "--url", &url, "--keydb", &dest]);
        assert_eq!(out.status.code(), Some(1), "{url}: {}", text(&out));
        assert!(!std::path::Path::new(&dest).exists());
    }
}

#[test]
fn info_without_a_url_or_with_an_unknown_scheme_fails() {
    let out = run(&["info"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let out = run(&["info", "bogus://thing"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
}

#[test]
fn info_on_a_stream_container_parses_flags_and_reports_an_unreadable_file() {
    let s = Scratch::new("mkv");
    let missing = format!("mkv://{}", s.path("missing.mkv"));
    let out = run(&["info", &missing, "--help"]);
    assert!(out.status.success(), "{}", text(&out));
    let out = run(&["info", &missing, "--bogus"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("--bogus"), "{}", text(&out));
    let out = run(&["info", &missing]);
    assert!(!out.status.success(), "{}", text(&out));
    std::fs::write(s.0.join("junk.m2ts"), vec![0u8; 4096]).unwrap();
    let junk = format!("m2ts://{}", s.path("junk.m2ts"));
    let out = run(&["info", &junk]);
    assert!(out.status.code().is_some(), "{}", text(&out));
}

fn bdmv_folder(s: &Scratch) -> std::path::PathBuf {
    let root = s.0.join("backup");
    std::fs::create_dir_all(root.join("BDMV").join("PLAYLIST")).unwrap();
    std::fs::create_dir_all(root.join("BDMV").join("STREAM")).unwrap();
    root
}

#[test]
fn a_lone_source_url_lists_the_disc_like_info() {
    let s = Scratch::new("lone");
    let root = bdmv_folder(&s);
    let out = run(&[&format!("dir://{}", root.display()), "--quiet"]);
    assert!(out.status.success(), "{}", text(&out));
    let out = run(&[&format!("dir://{}", root.display())]);
    assert!(text(&out).contains("No titles found"), "{}", text(&out));
}

#[test]
fn share_on_a_folder_prints_help_or_captures_into_the_working_directory() {
    let s = Scratch::new("share");
    let root = bdmv_folder(&s);
    let url = format!("dir://{}", root.display());
    let out = run(&["info", &url, "--share", "--help"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("--share"), "{}", text(&out));
    let out = freemkv()
        .current_dir(&s.0)
        .args(["info", &url, "--share", "--quiet"])
        .output()
        .expect("failed to run");
    assert!(out.status.code().is_some(), "{}", text(&out));
}

#[test]
fn arguments_with_no_url_are_a_usage_error() {
    let out = run(&["not-a-url", "neither"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
}
