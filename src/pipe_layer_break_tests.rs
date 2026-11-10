use super::layer_break_file;
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
