use super::*;
use crate::ku_fixtures::*;

// Resolve `answer`'s keys on a worker while polling T29's timer over `watched`; whether
// it ever expired, and the progress count. `observed`: the open's progress is threaded.
fn t29_over_resolve(answer: Answer, observed: bool) -> (bool, u64) {
    const WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
    let watched = libfreemkv::halt::Liveness::new();
    let p = watched.clone();
    let (ready, built) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let fx = bd_image(&[Some(K1)], 1);
        let f = factory(&[(answer, &[K1])], &Calls::default());
        let (scope, halt) = (KeyScope::Titles(vec![0]), Halt::new());
        ready.send(()).unwrap();
        match observed {
            true => resolve_observed(&fx.disc, &mut fx.source(), scope, &f, &halt, &p).0,
            false => super::resolve(&fx.disc, &mut fx.source(), scope, &f, None, Some(&halt)).0,
        }
    });
    built.recv().unwrap();
    let mut t29 = libfreemkv::halt::StallTimer::idle_only(WINDOW, &watched);
    let mut expired = false;
    while !worker.is_finished() {
        let stall = t29.poll(&watched);
        expired |= matches!(stall, libfreemkv::halt::Stall::Expired);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(worker.join().unwrap().is_ok(), "the keys resolved");
    (expired, watched.get())
}

// FT15f (stop design v5): "a key service trickling bytes between long gaps →
// `ResolveCtx::progress()` is bumped per chunk; T29 re-arms". Per spec.
#[test]
fn key_bytes_reach_the_probe_progress() {
    let (expired, bumps) = t29_over_resolve(Answer::Trickle, true);
    assert!(bumps >= u64::from(TRICKLE_CHUNKS), "{bumps} bumps");
    assert!(!expired, "T29 fired while key bytes moved");
}

// FT15g (b): "a parse or a CSS crack inside KU's `resolve`, taking 45 s (scaled)" → "the
// probe survives" (`busy()` per source call); the negative control shows T29 would fire.
#[test]
fn probe_survives_cpu_phases() {
    assert!(
        !t29_over_resolve(Answer::Slow, true).0,
        "T29 fired in a CPU phase"
    );
    assert!(t29_over_resolve(Answer::Slow, false).0, "negative control");
}
use libfreemkv::error::{E_CSS_NO_DISC_KEY, E_DECRYPT_FAILED};
use libfreemkv::keys::{KeyRing, KeyScope};

fn never_asked() -> libfreemkv::KeySourceFactory {
    std::sync::Arc::new(|| panic!("a raw copy must not build a key source"))
}

/// FK7 (KU §2.5): a raw copy (“`--raw`, GUI "Keep encrypted", GUI `raw_copy`”) has scope
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
    let clear = KeyRing::none();
    assert!(check_reopened(&clear, &other).is_ok(), "no keys: any disc");
}

fn dvd(css: Option<libfreemkv::css::CssState>, uncracked: bool) -> libfreemkv::Disc {
    libfreemkv::Disc {
        format: libfreemkv::DiscFormat::Dvd,
        encrypted: true,
        aacs: None,
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
    assert!(
        KS_5_CPI
            .text
            .contains("the data shall be considered encrypted")
    );
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let calls = Calls::default();
    let specs: &[(Answer, &[[u8; 16]])] = &[(Answer::Keydb, &[K1, K2])];
    let set = crate::ku_fixtures::resolve(&fx, KeyScope::Titles(vec![0]), specs, &calls).unwrap();
    let one = KeyScope::Titles(vec![0]);
    assert!(gate(&fx.disc, false, Some(&set), &one).is_ok());
    assert!(
        gate(&fx.disc, true, None, &KeyScope::None).is_ok(),
        "raw passes"
    );
    let cracked = libfreemkv::css::CssState {
        title_key: [1, 2, 3, 4, 5],
        crack_span: None,
    };
    let none = KeyRing::none();
    assert!(gate(&dvd(Some(cracked), false), false, Some(&none), &one).is_ok());
    let e = gate(&dvd(None, true), false, Some(&none), &one).unwrap_err();
    assert_eq!(
        e.code(),
        E_CSS_NO_DISC_KEY,
        "an uncracked CSS disc still refuses"
    );
}

/// KU §2.6: a best-effort (HD DVD) set shows `keys.hddvd_unverified`; a proven one does not.
#[test]
fn only_a_best_effort_set_is_marked_unverified() {
    let mut st = KeyRing::none().status();
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
