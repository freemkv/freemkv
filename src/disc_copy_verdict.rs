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
#[path = "disc_copy_verdict_tests.rs"]
mod tests;
