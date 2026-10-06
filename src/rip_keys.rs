//! The rip's AACS keys, for the CLI and the GUI alike (keys-upfront design §2.5, §3.3,
//! §4.2). KU §2.1 invariant 1: "Before R's first output byte, R holds a `KeyRing` K
//! from exactly one `KeyRing::resolve`", kept in memory only (invariant 5). Both
//! shells go through this module, so they make the same requests and reach the same
//! verdicts (FK3).

use freemkv_engine as fe;
use libfreemkv::keys::{KeyRing, KeyScope, KeySetStatus};
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
    session.take_disc().ok_or(Error::NoStreams)
}

// The GUI's (the CLI's copies scope their keys in the engine's `run`); the CLI binary builds
// this module without the GUI off macOS.
#[allow(dead_code)]
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
    seed: Option<&KeyRing>,
    halt: Option<&Halt>,
) -> (libfreemkv::Result<KeyRing>, Trace) {
    if scope == KeyScope::None {
        return (Ok(KeyRing::none()), Trace::new());
    }
    fe::keys::resolve_for_rip_traced(disc, reader, scope, sources, seed, halt)
}

/// [`resolve`] for an open (stop design v5 §4.3, "The open token"): under its `halt`, and
/// reporting to its `progress`, which each source call holds `busy()` and hands out as
/// `ResolveCtx::progress()` (T29).
// The GUI's; the CLI binary builds this module without the GUI off macOS.
#[allow(dead_code)]
pub fn resolve_observed(
    disc: &Disc,
    reader: &mut dyn SectorSource,
    scope: KeyScope,
    sources: &KeySourceFactory,
    halt: &Halt,
    progress: &libfreemkv::halt::Liveness,
) -> (libfreemkv::Result<KeyRing>, Trace) {
    if scope == KeyScope::None {
        return (Ok(KeyRing::none()), Trace::new());
    }
    fe::keys::resolve_for_rip_observed(disc, reader, scope, sources, None, Some(halt), progress)
}

/// How an image rip opens its source (KU §3.2 `open_image_with`).
pub struct ImageOpen {
    /// What the rip decrypts.
    pub scope: KeyScope,
    /// The set from Open: asked only for what it lacks (`KeyInput::Seeded`).
    pub seed: Option<KeyRing>,
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
pub fn check_reopened(set: &KeyRing, disc: &Disc) -> Result<(), Error> {
    if set.is_for(&disc.media_id()) {
        return Ok(());
    }
    tracing::error!(target: "freemkv::keys", "the reopened disc is not the one the rip's keys are for");
    Err(Error::DecryptFailed)
}

/// The one pre-flight decrypt gate (KU §3.5): AACS from the set, CSS from the disc.
pub fn gate(disc: &Disc, raw: bool, set: Option<&KeyRing>, scope: &KeyScope) -> Result<(), Error> {
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
#[path = "rip_keys_tests.rs"]
mod tests;
