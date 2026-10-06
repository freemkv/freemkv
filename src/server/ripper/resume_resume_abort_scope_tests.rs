fn title_lba(start_lba: u32, sector_count: u32) -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.extents.push(libfreemkv::disc::Extent {
        start_lba,
        sector_count,
    });
    t
}

#[test]
fn iso_resume_counts_out_of_title_loss() {
    // Out-of-title unreadable range only. For output_format=iso the
    // resume gate must see positive loss (whole-disc scope) — same as
    // a fresh ISO rip would, so both abort under abort_on_lost_secs=0.
    let bps = 8_250_000.0;
    let title = title_lba(1000, 1000);
    let bad = vec![(0u64, 50 * 2048)];
    let lost_secs = freemkv_engine::abort_lost_ms(/* output_is_iso */ true, &title, &bad, bps)
        / crate::server::util::MILLIS_PER_SEC;
    assert!(
        lost_secs > 0.0,
        "iso resume must count whole-disc (out-of-title) loss"
    );
}

#[test]
fn mkv_resume_ignores_out_of_title_loss() {
    // Same out-of-title range, mkv/m2ts output → in-title scope → 0,
    // so the resume gate proceeds to mux (matching fresh-rip mkv).
    let bps = 8_250_000.0;
    let title = title_lba(1000, 1000);
    let bad = vec![(0u64, 50 * 2048)];
    let lost_secs =
        freemkv_engine::abort_lost_ms(/* output_is_iso */ false, &title, &bad, bps)
            / crate::server::util::MILLIS_PER_SEC;
    assert_eq!(
        lost_secs, 0.0,
        "mkv resume must ignore out-of-title loss (in-title scope)"
    );
}
