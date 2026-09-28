//! The rip's AACS keys, for the CLI and the GUI alike (keys-upfront design §2.5, §3.3,
//! §4.2). KU §2.1 invariant 1: "Before R's first output byte, R holds a `ResolvedKeySet` K
//! from exactly one `ResolvedKeySet::resolve`", kept in memory only (invariant 5). Both
//! shells go through this module, so they make the same requests and reach the same
//! verdicts (FK3).

use freemkv_engine as fe;
use libfreemkv::keys::{KeyScope, KeySetStatus, ResolvedKeySet};
use libfreemkv::{Disc, Error, Halt, KeySourceFactory, SectorSource};

/// The per-source walk of a resolution: labels, node enums and counts, never key bytes.
pub type Trace = libfreemkv::aacs::trace::ResolutionTrace;

/// A test's stand-in for [`drive_scan`].
#[cfg(test)]
pub type FakeDrive = fn() -> Result<Disc, Error>;

#[cfg(test)]
thread_local! {
    static TEST_SOURCES: std::cell::RefCell<Option<KeySourceFactory>> =
        const { std::cell::RefCell::new(None) };
    static TEST_DRIVE: std::cell::RefCell<Option<FakeDrive>> =
        const { std::cell::RefCell::new(None) };
}

/// The key sources a rip asks, built from the user's settings or flags. The factory is
/// called once per resolve and dropped with it (KU LK7): nothing asks after `resolve`.
pub fn sources(params: &fe::KeyParams) -> KeySourceFactory {
    #[cfg(test)]
    if let Some(f) = TEST_SOURCES.with(|t| t.borrow().clone()) {
        return f;
    }
    fe::key_source_factory(params)
}

/// Run `f` with every [`sources`] call on this thread answered by `fake`.
#[cfg(test)]
pub fn with_sources<T>(fake: KeySourceFactory, f: impl FnOnce() -> T) -> T {
    TEST_SOURCES.with(|t| *t.borrow_mut() = Some(fake));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    TEST_SOURCES.with(|t| *t.borrow_mut() = None);
    out.unwrap_or_else(|p| std::panic::resume_unwind(p))
}

/// Run `f` with every [`drive_scan`] on this thread answered by `scan` (no drive in CI).
#[cfg(test)]
#[allow(dead_code)] // GUI tests only
pub fn with_drive<T>(scan: FakeDrive, f: impl FnOnce() -> T) -> T {
    TEST_DRIVE.with(|t| *t.borrow_mut() = Some(scan));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    TEST_DRIVE.with(|t| *t.borrow_mut() = None);
    out.unwrap_or_else(|p| std::panic::resume_unwind(p))
}

/// Scan the disc in the drive at `source` (`disc://` or `disc://DEVICE`) with NO key call
/// (KU §4.2): the disc's in-memory VID for an image whose keys need it (E7034), for the
/// GUI's Start after the disc is inserted. The CLI has no such step (USER 2026-09-28).
// The GUI's; the CLI binary builds this module without the GUI off macOS.
#[allow(dead_code)]
pub fn drive_scan(
    source: &str,
    credentials: Option<libfreemkv::DriveCredentials>,
) -> Result<Disc, Error> {
    #[cfg(test)]
    if let Some(scan) = TEST_DRIVE.with(|t| *t.borrow()) {
        let _ = (source, credentials);
        return scan();
    }
    let target = match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => libfreemkv::DeviceTarget::Path(p),
        _ => libfreemkv::DeviceTarget::Autodetect,
    };
    let mut session = fe::open_scan(target, credentials, false)?;
    Ok(session.take_disc().expect("scan populated the disc"))
}

/// What a disc→ISO copy decrypts (KU §2.5): the whole disc, or nothing for a raw copy
/// (“Raw copy (`--raw`, GUI "Keep encrypted", GUI `raw_copy`) | `None`: no key call”).
pub fn copy_scope(raw: bool) -> KeyScope {
    if raw {
        KeyScope::None
    } else {
        KeyScope::WholeDisc
    }
}

/// The rip's one up-front resolve over `reader` (KU §2.3), with its walk for the log of
/// why a key is missing. `KeyScope::None` builds no source at all (FK7). `seed`: a set this rip
/// already holds (the GUI's Open), whose keys join the pool first.
pub fn resolve(
    disc: &Disc,
    reader: &mut dyn SectorSource,
    scope: KeyScope,
    sources: &KeySourceFactory,
    seed: Option<&ResolvedKeySet>,
    halt: Option<&Halt>,
) -> (libfreemkv::Result<ResolvedKeySet>, Trace) {
    if scope == KeyScope::None {
        return (Ok(ResolvedKeySet::none()), Trace::new());
    }
    fe::keys::resolve_for_rip_traced(disc, reader, scope, sources, seed, halt)
}

/// How an image rip opens its source (KU §3.2 `open_image_with`).
pub struct ImageOpen {
    /// What the rip decrypts.
    pub scope: KeyScope,
    /// The set from Open: asked only for what it lacks (`KeyInput::Seeded`).
    pub seed: Option<ResolvedKeySet>,
    /// A drive's scan of the same disc (the GUI's Start after E7034): the VID comes with
    /// it and the image is not scanned (J14).
    pub drive_disc: Option<Disc>,
    pub halt: Option<Halt>,
}

/// Open an image and resolve its keys once, before any output (KU §3.2, §4.2): E7034 when
/// only the disc's VID can finish the key. Both shells' only image door.
pub fn open_image(
    src: &fe::ImageSource,
    sources: KeySourceFactory,
    o: ImageOpen,
) -> (libfreemkv::Result<fe::OpenedImage>, Trace) {
    let keys = match o.seed {
        Some(set) => fe::KeyInput::Seeded(sources, set),
        None => fe::KeyInput::Resolve(sources),
    };
    let opts = fe::OpenImageOptions {
        keys,
        disc: o.drive_disc,
        scope: Some(o.scope),
        vid: None,
        halt: o.halt,
    };
    fe::open_image_with_traced(src, opts)
}

/// A per-title reopen rescans the drive with no key call; the rescan must be the disc the
/// set was resolved for. KU §6: "A set used on the wrong disc … | E7013, plus an `error!`".
pub fn check_reopened(set: &ResolvedKeySet, disc: &Disc) -> Result<(), Error> {
    if set.is_for(disc) {
        return Ok(());
    }
    tracing::error!(target: "freemkv::keys", "the reopened disc is not the one the rip's keys are for");
    Err(Error::DecryptFailed)
}

/// The one pre-flight decrypt gate (KU §3.5): AACS from the set, CSS from the disc.
pub fn gate(
    disc: &Disc,
    raw: bool,
    set: Option<&ResolvedKeySet>,
    scope: &KeyScope,
) -> Result<(), Error> {
    libfreemkv::keys::check_decryptable(disc, raw, set, scope)
}

/// Whether a refusal is E7034: only the disc's VID can finish the key (KU §4.2, J11/J23).
#[allow(dead_code)] // the GUI's, as `drive_scan`
pub fn needs_disc(e: &Error) -> bool {
    e.code() == libfreemkv::error::E_AACS_VID_NEEDS_DISC
}

/// KU §2.6: a single-key HD DVD is keyed without proof, `best_effort`; say so.
pub fn best_effort_note(status: &KeySetStatus) -> Option<String> {
    status.best_effort.then(|| {
        crate::strings::get_or(
            "keys.hddvd_unverified",
            "HD DVD decryption is unverified — check the output.",
        )
    })
}

/// The walk as log lines, one per unlocker and key source: why a key is missing. English lives
/// here in the app layer; the library trace is typed enums only.
pub fn render_trace(trace: &Trace) -> Vec<String> {
    use libfreemkv::aacs::trace::{KeyNode, KeyOutcome as KO, UnlockOutcome};

    let mkb = |m: Option<u32>| match m {
        Some(n) => format!(" (MKBv{n})"),
        None => String::new(),
    };
    let mut lines = Vec::new();
    for step in &trace.unlock {
        let outcome = match step.outcome {
            UnlockOutcome::Unlocked => "UNLOCKED".to_string(),
            UnlockOutcome::FirmwareNotUnlockable => "firmware not unlockable".to_string(),
            UnlockOutcome::NoUsableHostCert { mkb: m } => format!("no usable host cert{}", mkb(m)),
            UnlockOutcome::CertRevoked { mkb: m } => format!("host cert revoked{}", mkb(m)),
            UnlockOutcome::HandshakeRejected => "handshake rejected".to_string(),
            UnlockOutcome::VidUnavailable => "Volume ID unavailable".to_string(),
        };
        lines.push(format!("unlock: {} > {outcome}", step.who));
    }
    for step in &trace.keys {
        let nodes = step.path.iter().map(|n| match n {
            KeyNode::MatchedDisc => "matched disc",
            KeyNode::NoEntry => "no entry",
            KeyNode::NoDerivableKey => "no derivable key",
            KeyNode::FoundUnitKeys => "found unit keys",
            KeyNode::FoundVuk => "found VUK",
            KeyNode::FoundMediaKey => "found media key",
            KeyNode::NeedVid => "need VID",
            KeyNode::VidFromUnlock => "VID from drive",
            KeyNode::VidFromKeydb => "VID from keydb",
            KeyNode::NoVid => "no VID",
            KeyNode::DerivedVuk => "derived VUK",
            KeyNode::DerivedUnitKeys => "derived unit keys",
        });
        let outcome = match step.outcome {
            KO::Resolved => "RESOLVED",
            KO::MissingVid => "MISSING VID",
            KO::NoKey => "NO KEY",
        };
        let mut parts = vec![step.who.clone()];
        parts.extend(nodes.map(str::to_string));
        parts.push(outcome.to_string());
        lines.push(format!("key: {}", parts.join(" > ")));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ku_fixtures::*;
    use libfreemkv::error::{E_CSS_NO_DISC_KEY, E_DECRYPT_FAILED};
    use libfreemkv::keys::{KeyScope, ResolvedKeySet};

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
        let clear = ResolvedKeySet::none();
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
        let set =
            crate::ku_fixtures::resolve(&fx, KeyScope::Titles(vec![0]), specs, &calls).unwrap();
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
        let none = ResolvedKeySet::none();
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
