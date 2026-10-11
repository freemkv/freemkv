//! A correction replan drops the superseded plan's outputs, never the capture it reads.
use super::*;

fn scratch(tag: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!("superseded-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn output(name: &str) -> staging::Output {
    staging::Output {
        filename: name.into(),
        ..Default::default()
    }
}

// ISO output records the captured image itself as the old plan's only output.
#[test]
fn replan_of_an_iso_output_keeps_the_captured_image() {
    let dir = scratch("iso");
    let iso = dir.join("Foo.iso");
    let map = dir.join("Foo.iso.mapfile");
    std::fs::write(&iso, b"image").unwrap();
    std::fs::write(&map, b"map").unwrap();
    remove_superseded_outputs(
        "_mux",
        &dir,
        &[output("Foo.iso")],
        &[output("Foo_S01E01.mkv"), output("Foo_S01E02.mkv")],
        &[&iso, &map],
    );
    assert!(iso.exists(), "the replan's source image was deleted");
    assert!(map.exists());
}

#[test]
fn replan_drops_outputs_the_new_plan_does_not_name() {
    let dir = scratch("mkv");
    let iso = dir.join("Foo.iso");
    std::fs::write(&iso, b"image").unwrap();
    std::fs::write(dir.join("Foo.mkv"), b"old").unwrap();
    std::fs::write(dir.join("Foo_S01E01.mkv"), b"kept").unwrap();
    remove_superseded_outputs(
        "_mux",
        &dir,
        &[output("Foo.mkv"), output("Foo_S01E01.mkv")],
        &[output("Foo_S01E01.mkv")],
        &[&iso],
    );
    assert!(!dir.join("Foo.mkv").exists());
    assert!(dir.join("Foo_S01E01.mkv").exists());
    assert!(iso.exists());
}
