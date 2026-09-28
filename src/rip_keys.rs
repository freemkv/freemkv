//! The rip's AACS keys, for the CLI and the GUI alike (keys-upfront design §2.5, §3.3,
//! §4.2). KU §2.1 invariant 1: "Before R's first output byte, R holds a `ResolvedKeySet` K
//! from exactly one `ResolvedKeySet::resolve`", kept in memory only (invariant 5). Both
//! shells go through this module, so they make the same requests and reach the same
//! verdicts (FK3).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ku_fixtures::*;
    use libfreemkv::error::{E_CSS_NO_DISC_KEY, E_DECRYPT_FAILED};
    use libfreemkv::keys::{KeyScope, ResolvedKeySet};

    fn never_asked() -> libfreemkv::KeySourceFactory {
        std::sync::Arc::new(|| panic!("a raw copy must not build a key source"))
    }

    /// FK7 (KU §2.5): a raw copy ("--raw, GUI 'Keep encrypted', GUI raw_copy") has scope
    /// `None`: "no key call". The source factory is never even built.
    #[test]
    fn raw_copy_makes_no_key_request() {
        assert_eq!(copy_scope(true), KeyScope::None);
        assert_eq!(copy_scope(false), KeyScope::WholeDisc);
        let fx = bd_image(&[Some(K1)], 1);
        let (set, trace) = super::resolve(
            &fx.disc,
            &mut fx.source(),
            copy_scope(true),
            &never_asked(),
            None,
            None,
        );
        let set = set.expect("a raw copy needs no key");
        assert!(!set.is_aacs(), "no key set for a raw copy");
        assert!(trace.keys.is_empty(), "no source walked");
    }

    /// FK4 (KU §3.3): every per-title reopen checks the rescan against the rip's one set;
    /// a different disc is "a set used on the wrong disc" (KU §6): E7013, never a resolve.
    #[test]
    fn per_title_reopen_checks_identity() {
        let fx = bd_image(&[Some(K1)], 1);
        let calls = Calls::default();
        let set = crate::ku_fixtures::resolve(
            &fx,
            KeyScope::Titles(vec![0]),
            &[(Answer::Keydb, &[K1])],
            &calls,
        )
        .unwrap();
        assert_eq!(check_reopened(&set, &fx.disc).map_err(|e| e.code()), Ok(()));
        let mut other = rescan(&fx);
        other.aacs.as_mut().unwrap().disc_hash = "0x".to_string() + &"ab".repeat(20);
        let e = check_reopened(&set, &other).unwrap_err();
        assert_eq!(e.code(), E_DECRYPT_FAILED, "{e}");
        assert_eq!(calls.len(), 1, "the reopen asked nothing");
        let clear = ResolvedKeySet::none();
        assert!(check_reopened(&clear, &other).is_ok(), "no keys: any disc");
    }

    fn dvd(css: Option<libfreemkv::css::CssState>, uncracked: bool) -> libfreemkv::Disc {
        libfreemkv::Disc {
            format: libfreemkv::DiscFormat::Dvd,
            encrypted: true,
            css,
            css_error: uncracked.then_some(libfreemkv::Error::CssNoDiscKey),
            content_format: libfreemkv::ContentFormat::MpegPs,
            ..bd_image(&[None], 1).disc
        }
    }

    /// FK5 (KU §3.5): the one pre-flight gate every CLI and GUI site uses. AACS from the
    /// set (KS-5, "shall be considered encrypted" unless proven otherwise); DVD CSS from
    /// `disc.css` / `css_error` "exactly as `Disc::ensure_decryptable_keys` does" (coord 7).
    #[test]
    fn keyed_rip_passes_every_cli_and_gui_gate() {
        use libfreemkv::spec::keys::KS_5_CPI;
        assert!(KS_5_CPI.text.contains("the data shall be considered encrypted"));
        let fx = bd_image(&[Some(K1), Some(K2)], 2);
        let calls = Calls::default();
        let specs: &[(Answer, &[[u8; 16]])] = &[(Answer::Keydb, &[K1, K2])];
        let set = crate::ku_fixtures::resolve(&fx, KeyScope::Titles(vec![0]), specs, &calls)
            .unwrap();
        let one = KeyScope::Titles(vec![0]);
        assert!(gate(&fx.disc, false, Some(&set), &one).is_ok());
        let wider = KeyScope::Titles(vec![0, 1]);
        let e = gate(&fx.disc, false, Some(&set), &wider).unwrap_err();
        assert_eq!(e.code(), E_DECRYPT_FAILED, "a scope outside the set is a caller bug");
        assert!(gate(&fx.disc, true, None, &KeyScope::None).is_ok(), "raw passes");
        let cracked = libfreemkv::css::CssState {
            title_key: [1, 2, 3, 4, 5],
            crack_span: None,
        };
        let none = ResolvedKeySet::none();
        assert!(gate(&dvd(Some(cracked), false), false, Some(&none), &one).is_ok());
        let e = gate(&dvd(None, true), false, Some(&none), &one).unwrap_err();
        assert_eq!(e.code(), E_CSS_NO_DISC_KEY, "an uncracked CSS disc still refuses");
    }

    /// KU §2.6: a best-effort (HD DVD) set shows `keys.hddvd_unverified`; a proven one does not.
    #[test]
    fn only_a_best_effort_set_is_marked_unverified() {
        let mut st = ResolvedKeySet::none().status();
        assert_eq!(best_effort_note(&st), None);
        st.best_effort = true;
        let note = best_effort_note(&st).expect("an unverified HD DVD key says so");
        assert!(note.contains("HD DVD"), "{note}");
    }

    /// E7034 (KU §4.2, J11) is told apart by code, the same way in both shells.
    #[test]
    fn only_e7034_needs_the_disc() {
        assert!(needs_disc(&libfreemkv::Error::AacsVidNeedsDisc));
        assert!(!needs_disc(&libfreemkv::Error::WholeDiscKeyMissing));
        let e = libfreemkv::Error::NoDiscKey {
            disc_hash: String::new(),
        };
        assert!(!needs_disc(&e));
    }
}
