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

/// A decrypted copy that keeps `AACS/Unit_Key_RO.inf` says so: some players then treat it
/// as encrypted. `None` once it was renamed (or there was nothing to rename). Only an ISO
/// whose directory was unreadable can be renamed by a later run, so only it says so.
///
/// `get_or` with literal keys: new keys, and the key-drift check reads them as written.
pub fn names_note(
    names: Option<&libfreemkv::whole_disc::DecryptedNames>,
    iso: bool,
) -> Option<String> {
    use libfreemkv::whole_disc::{DecryptedNames, NamesKept};
    let DecryptedNames::Kept { cause } = names? else {
        return None;
    };
    let mut note = crate::strings::get_or(
        "rip.aacs_key_file_kept",
        "The copy keeps AACS/Unit_Key_RO.inf, so some players (such as VLC) may treat it as encrypted. Its content is decrypted.",
    );
    if iso && *cause == NamesKept::Damaged {
        note.push(' ');
        note.push_str(&crate::strings::get_or(
            "rip.aacs_key_file_kept_retry",
            "A resumed run that reads the damaged area renames it to Unit_Key_RO.bak.",
        ));
    }
    Some(note)
}

#[cfg(test)]
#[path = "disc_copy_verdict_tests.rs"]
mod tests;
