// Catches the mutation removing catch_unwind from ANY worker spawn site.
// The claim is taken before spawn, so the worker owns clearing it even
// on panic; the scan site once lacked this, 409ing a device forever.
#[test]
fn every_worker_spawn_site_catches_its_panic() {
    let src = crate::server::util::source_lf(include_str!("web.rs"));
    // Match the production call shape only. (This file's test modules are
    // interleaved with production code, so a bare name match would also
    // count this test's own string literals.)
    const SITE: &str = "if let Err(e) = ripper::spawn_rip_thread(";
    let mut sites = 0;
    for (idx, _) in src.match_indices(SITE) {
        sites += 1;
        // The closure body follows the call; a spawn site's panic handling
        // must appear before the next spawn site (or the end of the file).
        let rest = &src[idx..];
        let window_end = rest[1..].find(SITE).map(|i| i + 1).unwrap_or(rest.len());
        assert!(
            rest[..window_end].contains("catch_unwind"),
            "a worker spawn site with no catch_unwind leaves the device's \
                 claim set forever if the worker panics; site {sites}"
        );
    }
    assert!(sites >= 2, "expected both web spawn sites; found {sites}");
}
