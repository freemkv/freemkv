//! On-disk state written by v1.8.3, installed under a temp root for the upgrade tests.
//!
//! `testdata/upgrade-1.8.3/v1.8.3` was emitted by the v1.8.3 tag's own writers (config
//! save, staging hand-off, queue, links, kept-remux sidecar, engine mapfile).
//! `dev-0eb9fd5` holds the delivery journal, queue and matches of the post-1.8.3 build
//! the live Library ran, anonymised. Every absolute path reads `@ROOT@`.
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

pub(crate) const OLD_STAMP: &str = "freemkv 1.8.3 (g956a0d3)";

pub(crate) fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/server/testdata/upgrade-1.8.3")
        .join(rel)
}

/// A canonical temp root: the journal compares canonical paths (macOS `/var` is a link).
pub(crate) fn root() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    (t, root)
}

/// `rel` of the fixture with `@ROOT@` bound to `root`, as text.
pub(crate) fn text(rel: &str, root: &Path) -> String {
    let raw = std::fs::read_to_string(fixture(rel)).unwrap();
    let escaped = serde_json::to_string(&root.to_string_lossy()).unwrap();
    raw.replace("@ROOT@", &escaped[1..escaped.len() - 1])
}

/// Copy fixture subtree `rel` to `root/dest`, binding `@ROOT@`; media files are copied as is.
pub(crate) fn install(rel: &str, root: &Path, dest: &str) {
    let from = fixture(rel);
    let entries = if from.is_file() {
        vec![from.clone()]
    } else {
        walk(&from)
    };
    for entry in entries {
        let leaf = entry.strip_prefix(&from).unwrap();
        let to = if leaf.as_os_str().is_empty() {
            root.join(dest)
        } else {
            root.join(dest).join(leaf)
        };
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        let sub = if leaf.as_os_str().is_empty() {
            PathBuf::from(rel)
        } else {
            Path::new(rel).join(leaf)
        };
        if entry.extension().is_some_and(|e| e == "mkv") {
            std::fs::copy(&entry, &to).unwrap();
        } else {
            std::fs::write(&to, text(&sub.to_string_lossy(), root)).unwrap();
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

pub(crate) fn old_mkv() -> Vec<u8> {
    crate::server::library::probe::testmkv::mkv(OLD_STAMP, Some(60.0), Some(58), true)
}

/// The ISOs and MKVs v1.8.3 left beside the fixture's state files, under `prefix`.
pub(crate) fn media(root: &Path, prefix: &str) {
    let list: Vec<(String, u64, String)> =
        serde_json::from_str(&text("v1.8.3/media.json", root)).unwrap();
    for (rel, size, kind) in list.into_iter().filter(|m| m.0.starts_with(prefix)) {
        let p = root.join(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let bytes = if kind == "mkv" {
            old_mkv()
        } else {
            vec![0u8; size as usize]
        };
        assert_eq!(bytes.len() as u64, size, "{rel}");
        std::fs::write(p, bytes).unwrap();
    }
}

/// The v1.8.3 library folder (`out/`) from `tree.json`.
pub(crate) fn library_tree(root: &Path) {
    let tree: Vec<(String, String)> =
        serde_json::from_str(&std::fs::read_to_string(fixture("v1.8.3/tree.json")).unwrap())
            .unwrap();
    for (rel, kind) in tree {
        let p = root.join("out").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let bytes = if kind == "mkv" {
            old_mkv()
        } else {
            b"iso".to_vec()
        };
        std::fs::write(p, bytes).unwrap();
    }
}

pub(crate) fn set_mtime(path: &Path, secs: u64, nanos: u32) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::new(secs, nanos))
        .unwrap();
}

/// The v1.8.3 settings, as `Config::load` reads them from `root/config`.
pub(crate) fn config(root: &Path) -> crate::server::config::Config {
    let mut cfg = crate::server::config::Config {
        autorip_dir: root.join("config").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let saved: serde_json::Value =
        serde_json::from_str(&text("v1.8.3/config/settings.json", root)).unwrap();
    crate::server::settings_schema::load_into(&mut cfg, &saved);
    cfg
}
