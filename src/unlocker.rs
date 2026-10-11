//! The unlocker that claims a drive, one rule for the CLI's report and the server's log.

/// The unlocker that handled the disc: the first match in the registry-derived
/// runtime results of `Disc::unlocker_matrix` (the full matrix stays with diagnostics).
pub fn matched_unlocker(matrix: &[(&'static str, bool)]) -> Option<&'static str> {
    matrix.iter().find_map(|(name, ok)| ok.then_some(*name))
}
