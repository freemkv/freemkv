use super::xdg;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const HOME: &str = "/srv/u";
const DIRS: &str = "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Bureau\"\nXDG_VIDEOS_DIR=\"$HOME/Vidéos\"\n";

fn env(s: &str) -> Option<OsString> {
    Some(OsString::from(s))
}

#[test]
fn data_home_defaults_and_ignores_relative_or_empty() {
    let h = Path::new(HOME);
    let dflt = PathBuf::from("/srv/u")
        .join(concat!(".", "local"))
        .join("share");
    assert_eq!(xdg::data_home(None, h), dflt);
    assert_eq!(xdg::data_home(env(""), h), dflt);
    assert_eq!(xdg::data_home(env("rel/data"), h), dflt);
    assert_eq!(xdg::data_home(env("/srv/d"), h), PathBuf::from("/srv/d"));
}

#[test]
fn config_home_defaults_and_ignores_relative() {
    let h = Path::new(HOME);
    assert_eq!(xdg::config_home(None, h), PathBuf::from("/srv/u/.config"));
    assert_eq!(
        xdg::config_home(env("cfg"), h),
        PathBuf::from("/srv/u/.config")
    );
    assert_eq!(xdg::config_home(env("/etc/u"), h), PathBuf::from("/etc/u"));
}

#[test]
fn videos_dir_reads_the_localized_entry_from_user_dirs() {
    let h = Path::new(HOME);
    assert_eq!(
        xdg::videos_dir(None, Some(DIRS), h),
        PathBuf::from("/srv/u/Vidéos")
    );
}

#[test]
fn videos_dir_precedence_and_fallbacks() {
    let h = Path::new(HOME);
    assert_eq!(
        xdg::videos_dir(env("/mnt/v"), Some(DIRS), h),
        PathBuf::from("/srv/u/Vidéos"),
        "user-dirs.dirs overrides a (possibly stale) exported variable"
    );
    assert_eq!(
        xdg::videos_dir(env("/mnt/v"), Some("XDG_MUSIC_DIR=\"$HOME/M\"\n"), h),
        PathBuf::from("/mnt/v"),
        "the environment still applies when the file has no Videos entry"
    );
    assert_eq!(
        xdg::videos_dir(env("/mnt/v"), Some("XDG_VIDEOS_DIR=\"$HOME\"\n"), h),
        PathBuf::from("/srv/u/Videos"),
        "a file entry disabling the dir overrides the environment too"
    );
    assert_eq!(
        xdg::videos_dir(None, Some("XDG_VIDEOS_DIR=\"/data/films\"\n"), h),
        PathBuf::from("/data/films")
    );
    assert_eq!(
        xdg::videos_dir(None, Some("XDG_VIDEOS_DIR=\"$HOME/\"\n"), h),
        PathBuf::from("/srv/u/Videos"),
        "an entry equal to $HOME means disabled"
    );
    assert_eq!(
        xdg::videos_dir(None, None, h),
        PathBuf::from("/srv/u/Videos")
    );
    assert_eq!(
        xdg::videos_dir(env("relative"), None, h),
        PathBuf::from("/srv/u/Videos")
    );
}
