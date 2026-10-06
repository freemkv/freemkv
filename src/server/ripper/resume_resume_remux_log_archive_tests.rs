use super::*;

fn tmpdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!(
            "autorip-resume-archive-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(p.join("logs")).unwrap();
    p
}

// Regression: resume_remux did not archive the prior session's per-device log on entry
// (unlike scan_disc/rip_disc), so "scan then resume" interleaved log entries.
#[test]
fn resume_remux_archives_prior_device_log() {
    // Held for the whole test: AUTORIP_DIR is process-wide and cargo runs
    // tests in parallel, so re-pointing it without this guard corrupts
    // concurrent tests. `env_guard()` also RESTORES the prior value on drop.
    let _guard = crate::server::log::env_guard();
    let d = tmpdir();
    // Route logs to the tempdir for this test. SAFETY: env access in
    // tests; the assertion that matters reads the in-memory ring
    // (keyed by the unique device name), not the env-routed file.
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }

    let dev = format!("test_resume_archive_sg_{}", std::process::id());

    // Seed a prior session's log line, as a scan/rip would leave behind.
    crate::server::log::device_log(&dev, "PRIOR-SESSION-SCAN-LINE");
    assert!(
        crate::server::log::get_device_log(&dev, 100)
            .iter()
            .any(|l| l.contains("PRIOR-SESSION-SCAN-LINE")),
        "prior line should be present before resume"
    );

    // A Remux classification pointing at a non-existent ISO so the
    // function archives + logs the resume line, then aborts on open.
    let class = ResumeClass::Remux {
        iso_path: d.join("does-not-exist.iso"),
        mapfile_path: d.join("does-not-exist.iso.mapfile"),
        display_name: "Nonexistent".to_string(),
        title_confident: None,
    };
    let cfg = Arc::new(RwLock::new(Config::default()));

    resume_remux(&cfg, &dev, class);

    let live = crate::server::log::get_device_log(&dev, 100);
    assert!(
        !live.iter().any(|l| l.contains("PRIOR-SESSION-SCAN-LINE")),
        "prior session line must be archived out of the live log, got: {:?}",
        live
    );
    assert!(
        live.iter().any(|l| l.contains("Auto-resume: re-muxing")),
        "live log should contain the new resume entry, got: {:?}",
        live
    );

    let _ = std::fs::remove_dir_all(&d);
}
