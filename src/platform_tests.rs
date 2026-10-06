/// The volume holding the temp dir always exists and always reports a
/// figure — a blank/zero here is the "Information panel is empty" defect.
#[test]
fn free_space_is_reported_for_a_real_directory() {
    let n = super::free_space_bytes(
        std::env::temp_dir()
            .to_str()
            .expect("temp dir is valid UTF-8"),
    );
    // `> 0` is satisfied by a hard-coded `Some(1)`. Any real volume with
    // room for a rip has far more than a mebibyte free.
    assert!(
        n.is_some_and(|b| b > 1_048_576),
        "no plausible free space reported: {n:?}"
    );
}

// `is_absolute` was untested; `starts_with('/')` called Windows paths
// relative, resetting the user's destination on load to a relative
// path — which writes rips next to the process CWD.
#[test]
fn is_absolute_rejects_empty_blank_and_relative_paths() {
    assert!(!super::is_absolute(""));
    assert!(!super::is_absolute("   "));
    assert!(!super::is_absolute("\t\n"));
    assert!(!super::is_absolute("Movies"));
    assert!(!super::is_absolute("../x"));
    assert!(!super::is_absolute("./Movies"));
    assert!(!super::is_absolute("Library/Application Support/freemkv"));
}

// The positive side, in each OS's native form: `C:\…` is not absolute
// on Unix, and a bare `/x` is not absolute on Windows (drive-relative),
// so a single shared rule would be wrong somewhere.
#[test]
fn is_absolute_accepts_the_platform_native_absolute_form() {
    #[cfg(unix)]
    {
        assert!(super::is_absolute("/opt/u/Movies"));
        assert!(super::is_absolute("  /opt/u/Movies  "));
        assert!(!super::is_absolute(r"C:\Users\u\Movies"));
    }
    #[cfg(windows)]
    {
        assert!(super::is_absolute(r"C:\Users\u\Movies"));
        assert!(super::is_absolute(r"  C:\Users\x\Movies  "));
        assert!(super::is_absolute(r"\\server\share\x"));
        assert!(!super::is_absolute(r"\Movies"));
    }
}

#[cfg(windows)]
#[test]
fn derived_paths_stay_absolute_without_userprofile_or_appdata() {
    // Serialized against the other env-mutating tests in this binary.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let profile = std::env::var_os("USERPROFILE");
    let appdata = std::env::var_os("APPDATA");
    unsafe {
        std::env::remove_var("USERPROFILE");
        std::env::remove_var("APPDATA");
    }

    let home = super::home_dir();
    let support = super::imp::support_dir();
    let dest = super::default_dest_dir();

    unsafe {
        if let Some(v) = profile {
            std::env::set_var("USERPROFILE", v);
        }
        if let Some(v) = appdata {
            std::env::set_var("APPDATA", v);
        }
    }

    assert!(home.is_absolute(), "home_dir went relative: {home:?}");
    assert!(
        support.is_absolute(),
        "support_dir went relative: {support:?}"
    );
    assert!(
        dest.is_absolute(),
        "default_dest_dir went relative: {dest:?}"
    );
    // An empty base is the specific shape of the bug: it makes the derived
    // path a bare suffix rather than a rooted location.
    assert!(support.ends_with("freemkv"));
    assert_ne!(
        support,
        std::path::Path::new("AppData")
            .join("Roaming")
            .join("freemkv")
    );
}

/// A destination that does not exist yet still resolves, via its nearest
/// existing ancestor — the normal case when naming an output file.
#[test]
fn a_not_yet_created_destination_resolves_via_its_parent() {
    let mut p = std::env::temp_dir();
    p.push("freemkv-does-not-exist-yet/out.mkv");
    assert!(super::free_space_bytes(p.to_str().unwrap()).is_some_and(|b| b > 0));
}

/// Nonsense input must not panic. A path holding a NUL byte names no file on any OS and
/// is `None`; an empty one means the current directory on Unix and is `None` on Windows.
#[test]
fn a_nul_path_is_none_and_an_empty_one_means_the_cwd() {
    assert_eq!(super::free_space_bytes("\0\0\0"), None);
    assert_eq!(super::free_space_bytes("/tmp/a\0b/out.mkv"), None);
    assert_eq!(
        super::free_space_bytes("").is_some(),
        cfg!(unix),
        "an empty path means the current directory on Unix"
    );
}

/// A real directory reports its volume's free bytes.
#[test]
fn a_real_temp_dir_reports_free_bytes() {
    let dir = crate::ku_fixtures::TempDir::new("free-space");
    let n = super::free_space_bytes(dir.path().to_str().expect("UTF-8 temp dir"));
    assert!(n.is_some_and(|b| b > 0), "{n:?}");
}

/// A missing leaf several levels deep is measured on the directory that does exist.
#[cfg(unix)]
#[test]
fn a_missing_leaf_probes_its_existing_ancestor() {
    let dir = crate::ku_fixtures::TempDir::new("free-space-leaf");
    let leaf = dir.path().join("not/yet/made/out.mkv");
    assert_eq!(
        super::imp::nearest_existing(&leaf, |q| q.exists()),
        dir.path()
    );
    assert!(super::free_space_bytes(leaf.to_str().unwrap()).is_some_and(|b| b > 0));
}

/// A relative destination that does not exist yet is measured on the current directory's
/// volume, never the root's.
#[cfg(unix)]
#[test]
fn a_missing_relative_destination_probes_the_current_directory() {
    use std::path::Path;
    let none = |_: &Path| false;
    assert_eq!(
        super::imp::nearest_existing(Path::new("out/new/file.mkv"), none),
        Path::new(".")
    );
    assert_eq!(
        super::imp::nearest_existing(Path::new("out/new/file.mkv"), |q| q == Path::new("out")),
        Path::new("out")
    );
    assert_eq!(
        super::imp::nearest_existing(Path::new("/gone/file.mkv"), none),
        Path::new("/")
    );
}

/// Unit tests build real `App`s (`Settings::load`, which renames an
/// unparseable file aside); they must never reach the user's real settings.
#[test]
fn unit_tests_never_resolve_the_real_support_dir() {
    let support = super::support_dir();
    assert!(
        support.starts_with(std::env::temp_dir()),
        "unit tests resolved a real support dir: {support:?}"
    );
}

/// The real resolution (bypassed by the redirect above) is still sane.
#[test]
fn the_real_support_dir_is_absolute_and_ends_in_freemkv() {
    let real = super::imp::support_dir();
    assert!(real.is_absolute() && real.ends_with("freemkv"), "{real:?}");
}
