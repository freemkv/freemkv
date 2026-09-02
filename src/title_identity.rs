//! What a title NUMBER actually referred to, so a selection survives a rescan.
//!
//! ONE definition, deliberately: the CLI's `pipe::resolve_scanned_title` and
//! the GUI's `engine::verify_title_identity` / `engine::remap_against` both
//! answer "is this still the same title?" from this single type. Declared by
//! both `lib.rs` and `main.rs` since `pipe` and `engine` are separate
//! compilations (`engine` is macOS-only in the binary).
//!
//! See docs/title-identity.md — why one definition, and why these fields.

/// The identity of one scanned title. Three fields, and no others:
///
/// - `playlist` — the disc's own name for the title, what `freemkv info` lists.
/// - `playlist_id` — the numeric form of the same, so a title whose name is
///   empty or unprintable still has something to compare and to show.
/// - `extents` — the SECTORS the title is read from, the physical identity of
///   the bytes.
///
/// Deliberately NOT used: the index, or duration/size. See
/// docs/title-identity.md for why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleIdentity {
    playlist: String,
    playlist_id: u16,
    extents: Vec<(u32, u32)>,
}

impl TitleIdentity {
    /// The identity of one scanned title.
    pub fn of(title: &libfreemkv::DiscTitle) -> Self {
        Self {
            playlist: title.playlist.clone(),
            playlist_id: title.playlist_id,
            extents: title
                .extents
                .iter()
                .map(|e| (e.start_lba, e.sector_count))
                .collect(),
        }
    }

    /// Short, log-safe rendering for a mismatch message. The playlist name is
    /// on-disc metadata, so it goes through `sanitize_display` before it can
    /// reach a terminal or a GUI line.
    pub fn describe(&self) -> String {
        let name = crate::strings::sanitize_display(&self.playlist);
        if name.is_empty() {
            format!("#{}", self.playlist_id)
        } else {
            format!("{} (#{})", name, self.playlist_id)
        }
    }
}
