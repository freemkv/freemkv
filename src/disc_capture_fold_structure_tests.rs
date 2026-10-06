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

// `capture`'s hard failures return their stderr lines, quiet or not, and write no zip.
#[test]
fn capture_fails_with_its_reason_when_there_is_no_structure_or_no_directory() {
    use libfreemkv::disc::DiscRegion;
    let disc = libfreemkv::Disc {
        volume_id: "T".to_string(),
        meta_title: None,
        format: libfreemkv::DiscFormat::BluRay,
        capacity_sectors: 0,
        capacity_bytes: 0,
        layers: 1,
        titles: Vec::new(),
        region: DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    };
    let base = std::env::temp_dir().join(format!("fmkv-capfail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    for quiet in [false, true] {
        let err = super::capture(&disc, &mut Blank, &base.join(format!("p{quiet}")), quiet)
            .err()
            .expect("a blank source has no structure");
        assert!(!err.is_empty(), "the reason must show, quiet={quiet}");
        assert!(!base.join(format!("p{quiet}/profile.zip")).exists());
    }
    // A profile directory that cannot be created is reported, not swallowed.
    let file = base.join("plain-file");
    std::fs::write(&file, b"x").unwrap();
    let err = super::capture(&disc, &mut Blank, &file.join("p"), true)
        .err()
        .expect("mkdir under a file fails");
    assert_eq!(err.len(), 1, "{err:?}");
    let _ = std::fs::remove_dir_all(&base);
}
