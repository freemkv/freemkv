use super::{Output, refuse_unstaged_titles};

// A per-test scratch dir under the system temp dir, removed on drop.
struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("fmkv-staged-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn title(start_lba: u32, sector_count: u32) -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.extents = vec![libfreemkv::Extent {
        start_lba,
        sector_count,
    }];
    t
}

// A staged image muxes only titles its scope holds; `-t` jobs and the default (None =
// title 0) are checked by extents.
#[test]
fn a_staged_image_muxes_only_titles_its_scope_holds() {
    let tmp = TmpDir::new();
    let iso = tmp.0.join("STAGED.iso");
    std::fs::write(&iso, vec![0u8; 8 * 2048]).unwrap();
    let mf = freemkv_engine::mapfile_path_for(&iso);
    std::fs::write(
        &mf,
        "# freemkv-scope: 0x0+0x2000\n0x0 ? 1\n0x0 0x2000 +\n0x2000 0x2000 ?\n",
    )
    .unwrap();
    let disc = libfreemkv::Disc {
        volume_id: "STAGED".into(),
        meta_title: None,
        format: libfreemkv::DiscFormat::Uhd,
        capacity_sectors: 8,
        capacity_bytes: 8 * 2048,
        layers: 1,
        titles: vec![title(0, 4), title(4, 4)],
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    };
    let out = Output::new(false, true);
    let job = |t: Option<usize>| vec![(t, String::new())];
    assert!(
        !refuse_unstaged_titles(&iso, &disc, &job(Some(0)), &out),
        "in scope"
    );
    assert!(
        !refuse_unstaged_titles(&iso, &disc, &job(None), &out),
        "default = title 0"
    );
    assert!(
        refuse_unstaged_titles(&iso, &disc, &job(Some(1)), &out),
        "E6022"
    );
    std::fs::remove_file(&mf).unwrap();
    assert!(
        !refuse_unstaged_titles(&iso, &disc, &job(Some(1)), &out),
        "not staged"
    );
}

// A staged image's title mux checks its scope before writing (a whole-disc output from
// it is refused by the engine's `run`, E6022).
#[test]
fn an_image_title_mux_checks_the_staged_scope_first() {
    let src = include_str!("pipe.rs").replace("\r\n", "\n");
    let body = |from: &str, to: &str| {
        let a = src.find(from).expect(from);
        src[a..a + src[a..].find(to).expect(to)].to_string()
    };
    let mux = body(
        "    let iso_disc = if is_disc",
        "        freemkv_engine::run_titles_with(",
    );
    assert!(
        mux.contains("refuse_unstaged_titles(&src, disc, &jobs, &out)"),
        "iso:// -> MKV"
    );
    // Its refusal is acted on (the run stops), and it comes before the title loop opens
    // any output.
    let call = mux
        .find("refuse_unstaged_titles(&src, disc, &jobs, &out)")
        .unwrap();
    assert!(
        mux[call..].lines().take(3).any(|l| l.trim() == "return 1;"),
        "a refused scope must end the run"
    );
    assert!(
        !mux[..call].contains("run_titles_with("),
        "the scope check must come before any title is muxed"
    );
}
