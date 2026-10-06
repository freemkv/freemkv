use super::disc_details;
use super::key_summary_tests::{aacs, disc};

#[test]
fn a_plain_unencrypted_disc_shows_type_capacity_region_and_titles() {
    let d = disc(false);
    let lines = disc_details(&d, "unencrypted");
    assert!(lines.contains(&"Type: Blu-ray".to_string()), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("Capacity:") && l.contains("layer")),
        "{lines:?}"
    );
    assert!(lines.contains(&"Region: free".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"Protection: unencrypted".to_string()),
        "{lines:?}"
    );
    assert!(lines.contains(&"Titles: 0".to_string()), "{lines:?}");
    // No AACS block on a disc with no aacs state.
    assert!(!lines.iter().any(|l| l.starts_with("MKB")), "{lines:?}");
}

/// The scan carries each title's size and the disc's capacity as numbers,
/// in canonical title order, for the Information panel's Source size row.
#[test]
fn a_disc_scan_carries_title_sizes_and_capacity() {
    let mut d = disc(false);
    d.capacity_bytes = 8_500_000_000;
    for size in [4_000_000_000, 0] {
        let mut t = libfreemkv::DiscTitle::empty();
        t.size_bytes = size;
        d.titles.push(t);
    }
    let sc = super::scanned_from_disc(&d, "unencrypted".into());
    assert_eq!(sc.title_sizes, vec![4_000_000_000, 0]);
    assert_eq!(sc.capacity_bytes, 8_500_000_000);
}

#[test]
fn zero_capacity_omits_the_capacity_line() {
    let mut d = disc(false);
    d.capacity_bytes = 0;
    let lines = disc_details(&d, "unencrypted");
    assert!(
        !lines.iter().any(|l| l.starts_with("Capacity:")),
        "{lines:?}"
    );
}

#[test]
fn each_region_shape_renders_its_own_line() {
    let mut bd = disc(false);
    bd.region = libfreemkv::disc::DiscRegion::BluRay(vec![
        libfreemkv::disc::BdRegion::A,
        libfreemkv::disc::BdRegion::C,
    ]);
    assert!(
        disc_details(&bd, "x").contains(&"Region: Blu-ray A/C".to_string()),
        "{:?}",
        disc_details(&bd, "x")
    );

    let mut dvd = disc(false);
    dvd.region = libfreemkv::disc::DiscRegion::Dvd(vec![1, 2]);
    assert!(
        disc_details(&dvd, "x").contains(&"Region: DVD 1,2".to_string()),
        "{:?}",
        disc_details(&dvd, "x")
    );

    let mut unknown = disc(false);
    unknown.region = libfreemkv::disc::DiscRegion::Unknown;
    assert!(
        disc_details(&unknown, "x").contains(&"Region: unknown".to_string()),
        "{:?}",
        disc_details(&unknown, "x")
    );

    // An empty region list falls through to no region line at all.
    let mut empty = disc(false);
    empty.region = libfreemkv::disc::DiscRegion::BluRay(vec![]);
    assert!(
        !disc_details(&empty, "x")
            .iter()
            .any(|l| l.starts_with("Region:")),
        "an empty region list emits no line"
    );
}

#[test]
fn the_aacs_block_shows_mkb_hash_and_a_non_zero_vid() {
    let mut d = disc(true);
    let mut a = aacs();
    a.mkb_version = Some(64);
    a.bus_encryption = true;
    a.disc_hash = "deadbeef".into();
    a.volume_id = [0xAB; 16];
    d.aacs = Some(a);

    let lines = disc_details(&d, "unlocked via keydb");
    assert!(lines.contains(&"MKB v64".to_string()), "{lines:?}");
    // Bus-encryption flag isn't surfaced any more; Type: Uhd carries it.
    assert!(
        !lines.iter().any(|l| l.contains("bus encryption")),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"Disc hash: deadbeef".to_string()),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "VID: 0xabababababababababababababababab"),
        "{lines:?}"
    );
    // Resolved key material never renders.
    assert!(!lines.iter().any(|l| l.contains("VUK")), "{lines:?}");
    assert!(
        !lines.iter().any(|l| l.trim_start().starts_with("CPS")),
        "{lines:?}"
    );
}

#[test]
fn an_all_zero_volume_id_prints_no_vid_line() {
    let mut d = disc(true);
    d.aacs = Some(aacs()); // volume_id defaults to all-zero
    assert!(
        !disc_details(&d, "x").iter().any(|l| l.starts_with("VID:")),
        "an unavailable (all-zero) VID must not print a line"
    );
}

// The GUI log is shared in bug reports: planted key bytes must never render,
// whatever the log detail level.
#[test]
fn the_detail_block_never_renders_the_vuk_or_any_unit_key() {
    let mut d = disc(true);
    d.aacs = Some(aacs());

    let text = disc_details(&d, "unlocked via keydb").join("\n");
    for secret in ["eeeeeeee", "11111111", "22222222"] {
        assert!(!text.contains(secret), "key bytes {secret} leaked:\n{text}");
    }
    assert!(!text.contains("VUK") && !text.contains("CPS"), "{text}");
}
