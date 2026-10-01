//! What a finished `disc:// -> iso://` copy still has to tell the user when it
//! did not recover the whole disc — one renderer, both shells. Declared in
//! both crate roots (see `lossy.rs`) so the CLI's `pipe` and the GUI's
//! `engine` answer with the same wording instead of two that can drift apart.
//!
//! Losing some sectors does not fail a single-pass copy: the image is kept
//! and is usable. Only recovering NOTHING is a failure.

/// NOTHING readable came back: the ISO is unusable. Both shells KEEP it (so a
/// later run can inspect or resume it) and fail with this line.
pub fn iso_no_data_error(unreadable_bytes: u64) -> String {
    crate::strings::fmt(
        "rip.no_data",
        &[(
            "unreadable",
            &format!("{:.1}", unreadable_bytes as f64 / 1_048_576.0),
        )],
    )
}

/// A single-pass copy short of some sectors is still a SUCCESS — the image is
/// kept — but the operator needs to be told another run can recover more, and
/// that it resumes (from the mapfile) rather than starting over — true even
/// right after a `--multipass` run, which only retries once per invocation.
///
/// `get_or` (not `get`): this key is new and not yet in the `freemkv-i18n`
/// tag this crate pins, so the English fallback is what ships until it is.
pub fn retry_with_multipass_hint() -> String {
    crate::strings::get_or(
        "rip.retry_with_multipass",
        "Re-run with --multipass to try to read more; it resumes from the mapfile.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_no_data_error_names_the_unreadable_amount() {
        crate::strings::set_locale("en");
        let msg = iso_no_data_error(1_048_576);
        assert!(msg.contains("1.0"), "unreadable MB must appear: {msg}");
        // MiB with one decimal: 3.5 MiB, not 3.67 MB or 3670016 bytes.
        let msg = iso_no_data_error(3_670_016);
        assert!(msg.contains("3.5"), "{msg}");
    }

    #[test]
    fn the_retry_hint_is_not_empty() {
        crate::strings::set_locale("en");
        assert!(retry_with_multipass_hint().contains("--multipass"));
    }
}
