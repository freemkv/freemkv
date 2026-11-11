use super::{layer_break_file, layer_break_file_wanted};
use libfreemkv::{DiscFormat, LayerMap, TrackPath};
use std::path::Path;

fn disc(format: DiscFormat, map: Option<LayerMap>) -> libfreemkv::Disc {
    libfreemkv::Disc {
        volume_id: "MOVIE".into(),
        meta_title: None,
        format,
        capacity_sectors: 4_000_000,
        capacity_bytes: 4_000_000 * 2048,
        layers: map.as_ref().map_or(1, |m| m.layers),
        titles: vec![],
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        layer_map: map,
        content_format: libfreemkv::ContentFormat::DvdPs,
    }
}

fn dual(layer_break: Option<u32>) -> Option<LayerMap> {
    Some(LayerMap {
        layers: 2,
        layer_break,
        track_path: Some(TrackPath::Opposite),
    })
}

#[test]
fn a_dual_layer_dvd_gets_a_dvd_file_beside_the_iso() {
    let d = disc(DiscFormat::Dvd, dual(Some(2_084_960)));
    let (side, body) = layer_break_file(&d, Path::new("/out/Movie.iso")).unwrap();
    assert_eq!(side, Path::new("/out/Movie.dvd"));
    assert_eq!(body, "LayerBreak=2084960\r\nMovie.iso\r\n");
}

#[test]
fn no_dvd_file_without_a_known_dvd_break() {
    let iso = Path::new("/out/Movie.iso");
    assert!(layer_break_file(&disc(DiscFormat::Dvd, dual(None)), iso).is_none());
    assert!(layer_break_file(&disc(DiscFormat::Dvd, None), iso).is_none());
    // HD DVD reports a break, but takes no ImgBurn `.dvd` file; neither does BD.
    assert!(layer_break_file(&disc(DiscFormat::HdDvd, dual(Some(9))), iso).is_none());
    assert!(layer_break_file(&disc(DiscFormat::BluRay, dual(Some(9))), iso).is_none());
}

#[test]
fn an_image_named_dvd_is_never_overwritten() {
    let d = disc(DiscFormat::Dvd, dual(Some(2_084_960)));
    assert!(layer_break_file(&d, Path::new("/out/Movie.dvd")).is_none());
}

#[test]
fn the_dvd_file_is_written_next_to_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Movie.iso");
    let d = disc(DiscFormat::Dvd, dual(Some(2_084_960)));
    let _ = crate::output::capture(|| {
        super::write_layer_break_file(&d, &iso, &crate::output::Output::new(false, true))
    });
    let body = std::fs::read_to_string(dir.path().join("Movie.dvd")).unwrap();
    assert_eq!(body, "LayerBreak=2084960\r\nMovie.iso\r\n");
}

#[test]
fn only_a_single_dual_layer_map_with_a_break_names_a_file() {
    let one = Some(LayerMap {
        layers: 1,
        layer_break: Some(2_084_960),
        track_path: None,
    });
    let iso = Path::new("/out/Movie.iso");
    assert!(layer_break_file(&disc(DiscFormat::Dvd, one), iso).is_none());
    // An image without an extension still gets `<stem>.dvd`, naming the image as written.
    let d = disc(DiscFormat::Dvd, dual(Some(7)));
    let (side, body) = layer_break_file(&d, Path::new("/out/Movie")).unwrap();
    assert_eq!(side, Path::new("/out/Movie.dvd"));
    assert_eq!(body, "LayerBreak=7\r\nMovie\r\n");
}

fn copied(halted: bool, good: u64) -> freemkv_engine::CopyResult {
    freemkv_engine::CopyResult {
        bytes_total: 100,
        bytes_good: good,
        bytes_unreadable: 100 - good,
        bytes_pending: 0,
        recovered_this_pass: good,
        complete: !halted && good == 100,
        halted,
    }
}

// The `.dvd` file follows a kept image read from a drive, never an image of an image, a
// null copy, a stopped copy or one that recovered nothing.
#[test]
fn the_dvd_file_follows_only_a_kept_image_from_a_drive() {
    let image = freemkv_engine::Output::Image {
        path: "/out/Movie.iso".into(),
        null: false,
    };
    assert!(layer_break_file_wanted(
        &copied(false, 100),
        "disc://",
        &image
    ));
    assert!(layer_break_file_wanted(
        &copied(false, 60),
        "disc:///dev/sg3",
        &image
    ));
    assert!(!layer_break_file_wanted(
        &copied(true, 60),
        "disc://",
        &image
    ));
    assert!(!layer_break_file_wanted(
        &copied(false, 0),
        "disc://",
        &image
    ));
    assert!(!layer_break_file_wanted(
        &copied(false, 100),
        "iso:///in/Movie.iso",
        &image
    ));
    let null = freemkv_engine::Output::Image {
        path: "/out/Movie.iso".into(),
        null: true,
    };
    assert!(!layer_break_file_wanted(
        &copied(false, 100),
        "disc://",
        &null
    ));
    let tree = freemkv_engine::Output::Tree {
        path: "/out/Movie".into(),
    };
    assert!(!layer_break_file_wanted(
        &copied(false, 100),
        "disc://",
        &tree
    ));
}

#[test]
fn a_failed_write_is_reported_not_swallowed() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("missing").join("Movie.iso");
    let d = disc(DiscFormat::Dvd, dual(Some(2_084_960)));
    let ((), text) = crate::output::capture(|| {
        super::write_layer_break_file(&d, &iso, &crate::output::Output::new(false, true))
    });
    assert!(
        text.contains("Could not write the layer break file"),
        "{text}"
    );
    assert!(text.contains("Movie.dvd"), "{text}");
    assert!(!dir.path().join("missing").exists());
}
