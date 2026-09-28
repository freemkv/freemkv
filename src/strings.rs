// freemkv — i18n string loader. MIT — freemkv project.

// Loader lives in the shared `freemkv-i18n` crate (reused by `autorip`); this
// is a thin re-export so existing call sites keep working unchanged.
pub use freemkv_i18n::*;

/// A catalog string with a compiled-in English fallback, for keys this crate
/// knows that the pinned `freemkv-i18n` tag does not ship yet.
///
/// `freemkv_i18n::get` answers a catalog miss by returning the dotted path
/// itself, which is right for a translator and wrong for a user; `get_or`
/// returns `english` instead whenever `key` did not resolve. Once the key
/// ships in a tagged catalog, the `english` argument stops being reachable
/// and the call can collapse to a plain [`get`].
pub fn get_or(key: &str, english: &str) -> String {
    match get(key) {
        s if s == key => english.to_string(),
        s => s,
    }
}

/// [`get_or`] with `{placeholder}` substitution — the fallback half of
/// [`fmt`]. The substitution runs on whichever text won, so the English
/// stopgap and the translation take the same arguments and cannot drift.
pub fn fmt_or(key: &str, english: &str, args: &[(&str, &str)]) -> String {
    let mut s = get_or(key, english);
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), value);
    }
    s
}

// Lives here (not in `engine`) because this is the only module both the CLI
// (`disc_info`) and `engine` compile — putting it in `engine` broke the
// Linux CLI build, since `engine` is macOS-only under `main.rs`.
/// Strip control/escape characters from untrusted on-disc metadata (volume
/// label, title name, stream labels) before it is displayed.
///
/// Distinct from `engine::sanitize_label`, which makes a label safe as a
/// filename component and is lossier on purpose. This one removes only what
/// cannot be safely rendered, so a Japanese or Cyrillic label survives whole.
pub fn sanitize_display(s: &str) -> String {
    s.chars().filter(|&c| !is_unsafe_display_char(c)).collect()
}

/// Characters to strip before display: C0/C1 controls (including ESC, and the
/// newlines that would let a crafted disc forge a whole log line) AND the
/// Unicode format (Cf) characters `char::is_control()` misses —
/// bidirectional overrides/isolates (U+202A-202E, U+2066-2069), zero-width
/// spaces/joiners (U+200B-200F, U+2060-2064) and the BOM (U+FEFF), which can
/// reorder or hide how the rest of a line renders.
pub fn is_unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}')
}

/// Which shell renders an error: each may word the fix its own way (the CLI names its
/// command, the GUI its Settings action); the shared `error.<code>` text stays neutral,
/// because the freemkv-library server shows it too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontEnd {
    Cli,
    // The CLI binary builds this module without the GUI (off macOS).
    #[allow(dead_code)]
    Gui,
}

/// The catalog key to render `code` (`"E7013"`) with on `front`: its own `error_cli.` /
/// `error_gui.` wording when the catalog has one, else the shared `error.` text.
pub fn front_end_error_key(front: FrontEnd, code: &str) -> String {
    let name = match front {
        FrontEnd::Cli => "cli",
        FrontEnd::Gui => "gui",
    };
    front_end_error_key_in(name, code, get)
}

fn front_end_error_key_in(front: &str, code: &str, lookup: impl Fn(&str) -> String) -> String {
    let own = format!("error_{front}.{code}");
    // A catalog miss returns the dotted path itself.
    if lookup(&own) != own {
        own
    } else {
        format!("error.{code}")
    }
}

#[cfg(test)]
mod front_end_error_tests {
    use super::*;

    /// A front end's own error wording wins when the catalog has it; the shared
    /// `error.<code>` text (front-end-neutral: the library server shows it too) otherwise.
    #[test]
    fn a_front_end_prefers_its_own_error_wording() {
        let catalog = |key: &str| match key {
            "error_cli.E7013" => "refresh it with: freemkv update-keys".to_string(),
            k => k.to_string(),
        };
        assert_eq!(
            front_end_error_key_in("cli", "E7013", catalog),
            "error_cli.E7013"
        );
        assert_eq!(
            front_end_error_key_in("gui", "E7013", catalog),
            "error.E7013"
        );
        assert_eq!(
            front_end_error_key_in("cli", "E7022", catalog),
            "error.E7022"
        );
    }

    /// The keydb-fixing codes name a step on every front end: the CLI its command
    /// (shared text before the neutral wording lands, its own after).
    #[test]
    fn the_cli_still_names_update_keys_for_every_keydb_code() {
        for code in ["E7000", "E7013", "E7026", "E8001", "E8002", "E8004"] {
            let text = get(&front_end_error_key(FrontEnd::Cli, code));
            assert!(text.contains("freemkv update-keys"), "{code}: {text}");
        }
    }
}
