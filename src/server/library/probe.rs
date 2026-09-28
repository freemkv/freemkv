//! Per-MKV facts: which freemkv muxed it, and the fast structural audit.
//!
//! Both are cached by path and keyed by size + mtime, so a file that changes
//! (a remux landed, someone replaced it) is re-read on the next look. The
//! muxed-with read is header-only; the audit also follows the SeekHead to the
//! Cues, never reading a cluster.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Size and mtime: when either moves, the cached facts are stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSig {
    pub size: u64,
    pub mtime_ns: i128,
}

impl FileSig {
    pub fn of(meta: &std::fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .map(|t| match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_nanos() as i128,
                Err(e) => -(e.duration().as_nanos() as i128),
            })
            .unwrap_or(0);
        Self {
            size: meta.len(),
            mtime_ns,
        }
    }

    pub fn stat(path: &Path) -> Option<Self> {
        std::fs::metadata(path).ok().map(|m| Self::of(&m))
    }
}

/// The running freemkv's version, which a current MKV carries in its writing-app stamp.
pub fn running_version() -> (u32, u32, u32) {
    let mut it = env!("CARGO_PKG_VERSION")
        .split(['.', '-'])
        .map(|p| p.parse().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// How a file's writing-app stamp compares with the running version.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MuxedWith {
    /// Muxed by this freemkv version.
    Current { version: String },
    /// Muxed by another freemkv version.
    Older { version: String },
    /// Written by something that is not a versioned freemkv.
    Other { app: String },
    /// The file carries no stamp (or could not be read).
    Unknown,
}

impl MuxedWith {
    pub fn from_app(app: Option<&str>, running: (u32, u32, u32)) -> Self {
        let Some(app) = app.map(str::trim).filter(|a| !a.is_empty()) else {
            return Self::Unknown;
        };
        match libfreemkv::parse_freemkv_version(app) {
            Some(v) => {
                let version = format!("{}.{}.{}", v.0, v.1, v.2);
                if v == running {
                    Self::Current { version }
                } else {
                    Self::Older { version }
                }
            }
            None => Self::Other {
                app: app.to_string(),
            },
        }
    }

    /// A remux would change it. An unstamped file is left alone.
    pub fn out_of_date(&self) -> bool {
        matches!(self, Self::Older { .. } | Self::Other { .. })
    }
}

/// A writing-app stamp cut down to "program version" for a table cell:
/// `"mkvmerge v96.0 ('It's My Life') 64-bit"` becomes `"mkvmerge 96.0"`,
/// `"freemkv 1.7.7 (gc8e67f1)"` becomes `"freemkv 1.7.7"`.
pub fn short_label(app: &str) -> String {
    let mut words = app.split_whitespace();
    let Some(name) = words.next() else {
        return String::new();
    };
    let version = words
        .map(|w| w.trim_start_matches(['v', 'V']))
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()) && w.contains('.'));
    match version {
        Some(v) => format!("{name} {v}"),
        None => name.to_string(),
    }
}

#[derive(Clone, Debug)]
struct Stamp {
    sig: FileSig,
    writing_app: Option<String>,
}

/// Which audit ran. Only the fast structural pass exists; a full decode would be
/// a second depth, provided by an image variant that bundles a decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDepth {
    Fast,
}

/// One finding of an audit.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditIssue {
    /// No EBML header: not a Matroska file.
    NotMkv,
    /// The header could not be parsed; `code` is the library's error code.
    Unreadable {
        code: Option<u16>,
    },
    NoVideo,
    NoDuration,
    /// No Cues index, so the runtime can only be taken from the header.
    NoCues,
    /// The last Cues entry is not where the header's Duration says the file ends.
    RuntimeMismatch {
        runtime_secs: f64,
        duration_secs: f64,
    },
}

impl AuditIssue {
    /// Whether this finding fails the file (a missing index only warns).
    pub fn fatal(&self) -> bool {
        !matches!(self, Self::NoCues)
    }
}

/// The result of auditing one MKV.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct AuditReport {
    pub depth: AuditDepth,
    pub ok: bool,
    pub issues: Vec<AuditIssue>,
    pub duration_secs: Option<f64>,
    pub runtime_secs: Option<f64>,
    pub video_tracks: usize,
    pub audio_tracks: usize,
    pub subtitle_tracks: usize,
    /// Video codecs, in track order (`"HEVC"`, `"AVC"`, ...).
    pub video: Vec<String>,
    /// Audio tracks as codec and language, in track order.
    pub audio: Vec<TrackFacts>,
    /// Subtitle languages, in track order.
    pub subtitles: Vec<String>,
}

/// One audio track as the Library shows it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TrackFacts {
    pub codec: String,
    pub language: String,
}

/// A Matroska CodecID as a reader would name it.
pub fn codec_name(id: &str) -> String {
    let known = [
        ("V_MPEGH/ISO/HEVC", "HEVC"),
        ("V_MPEG4/ISO/AVC", "AVC"),
        ("V_MPEG2", "MPEG-2"),
        ("V_MPEG1", "MPEG-1"),
        ("V_MS/VFW/FOURCC", "VC-1"),
        ("V_AV1", "AV1"),
        ("A_TRUEHD", "TrueHD"),
        ("A_MLP", "TrueHD"),
        ("A_DTS", "DTS"),
        ("A_EAC3", "E-AC-3"),
        ("A_AC3", "AC-3"),
        ("A_PCM", "PCM"),
        ("A_FLAC", "FLAC"),
        ("A_AAC", "AAC"),
        ("A_OPUS", "Opus"),
        ("A_MPEG/L3", "MP3"),
        ("A_MPEG/L2", "MP2"),
        ("S_HDMV/PGS", "PGS"),
        ("S_VOBSUB", "VobSub"),
        ("S_TEXT/UTF8", "SRT"),
    ];
    known
        .iter()
        .find(|(prefix, _)| id.starts_with(prefix))
        .map_or_else(|| id.to_string(), |(_, name)| (*name).to_string())
}

// Same slack the engine's verify allows between a runtime and a declared length.
const RUNTIME_SLACK_SECS: f64 = 10.0;
const RUNTIME_SLACK_FRACTION: f64 = 0.02;

/// Whether a read failure says something about the file (it is short, garbled
/// or not Matroska) rather than about the storage (EIO, ESTALE, a timeout, a
/// file that vanished). Only a verdict about the file may be cached.
pub fn is_verdict(e: &std::io::Error) -> bool {
    freemkv_engine::error_code(e).is_some()
        || matches!(
            e.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
        )
}

/// The fast pass: EBML magic, header, at least one video track, a Duration, and
/// the last Cues entry within max(10 s, 2 %) of that Duration. `None` when the
/// storage failed rather than the file: try again on the next pass.
pub fn audit_fast(path: &Path) -> Option<AuditReport> {
    let mut report = AuditReport {
        depth: AuditDepth::Fast,
        ok: false,
        issues: Vec::new(),
        duration_secs: None,
        runtime_secs: None,
        video_tracks: 0,
        audio_tracks: 0,
        subtitle_tracks: 0,
        video: Vec::new(),
        audio: Vec::new(),
        subtitles: Vec::new(),
    };
    let magic_ok = std::fs::File::open(path).and_then(|mut f| {
        let mut m = [0u8; 4];
        f.read_exact(&mut m)?;
        Ok(m == [0x1A, 0x45, 0xDF, 0xA3])
    });
    match magic_ok {
        Ok(true) => {}
        Ok(false) => {
            report.issues.push(AuditIssue::NotMkv);
            return Some(report);
        }
        Err(e) if is_verdict(&e) => {
            report.issues.push(AuditIssue::NotMkv);
            return Some(report);
        }
        Err(_) => return None,
    }
    let probe = std::fs::File::open(path)
        .map(std::io::BufReader::new)
        .and_then(libfreemkv::probe_mkv_with_cues);
    let probe = match probe {
        Ok(p) => p,
        Err(e) if is_verdict(&e) => {
            let code = freemkv_engine::error_code(&e);
            report.issues.push(AuditIssue::Unreadable { code });
            return Some(report);
        }
        Err(_) => return None,
    };
    use libfreemkv::MkvTrackKind as K;
    let count = |k: fn(&K) -> bool| probe.tracks.iter().filter(|t| k(&t.kind)).count();
    report.video_tracks = count(|k| matches!(k, K::Video));
    report.audio_tracks = count(|k| matches!(k, K::Audio));
    report.subtitle_tracks = count(|k| matches!(k, K::Subtitle));
    for t in &probe.tracks {
        match t.kind {
            K::Video => report.video.push(codec_name(&t.codec_id)),
            K::Audio => report.audio.push(TrackFacts {
                codec: codec_name(&t.codec_id),
                language: t.language.clone(),
            }),
            K::Subtitle => report.subtitles.push(t.language.clone()),
            K::Other(_) => {}
        }
    }
    report.duration_secs = probe.duration_secs;
    report.runtime_secs = probe.last_cue_secs;
    if report.video_tracks == 0 {
        report.issues.push(AuditIssue::NoVideo);
    }
    match (probe.duration_secs, probe.last_cue_secs) {
        (None, _) => report.issues.push(AuditIssue::NoDuration),
        (Some(_), None) => report.issues.push(AuditIssue::NoCues),
        (Some(d), Some(r)) => {
            let slack = RUNTIME_SLACK_SECS.max(d * RUNTIME_SLACK_FRACTION);
            if !d.is_finite() || !r.is_finite() || (d - r).abs() > slack {
                report.issues.push(AuditIssue::RuntimeMismatch {
                    runtime_secs: r,
                    duration_secs: d,
                });
            }
        }
    }
    report.ok = report.issues.iter().all(|i| !i.fatal());
    Some(report)
}

// The writing-app stamp of `path`. `Ok(None)` is a verdict (no stamp, or not
// a readable MKV); `Err` is a storage failure worth retrying.
fn read_stamp(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::File::open(path)
        .map(std::io::BufReader::new)
        .and_then(libfreemkv::probe_mkv)
    {
        Ok(p) => Ok(p.writing_app.or(p.muxing_app)),
        Err(e) if is_verdict(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The cache behind both facts. Filesystem work never happens under its locks.
#[derive(Default)]
pub struct ProbeCache {
    stamps: Mutex<HashMap<PathBuf, Stamp>>,
    audits: Mutex<HashMap<PathBuf, (FileSig, AuditReport)>>,
}

impl ProbeCache {
    /// The writing-app stamp of `path`, re-read whenever its size or mtime moved.
    /// `None` when the file cannot be stat'd.
    pub fn writing_app(&self, path: &Path) -> Option<Option<String>> {
        let sig = FileSig::stat(path)?;
        if let Some(s) = self.lock_stamps().get(path)
            && s.sig == sig
        {
            return Some(s.writing_app.clone());
        }
        let app = read_stamp(path).ok()?;
        self.lock_stamps().insert(
            path.to_path_buf(),
            Stamp {
                sig,
                writing_app: app.clone(),
            },
        );
        Some(app)
    }

    /// The cached stamp for `path` if it was read at `sig`. No filesystem I/O:
    /// `None` means "not read yet", `Some(None)` "read, and it has no stamp".
    pub fn cached_stamp(&self, path: &Path, sig: FileSig) -> Option<Option<String>> {
        self.lock_stamps()
            .get(path)
            .filter(|s| s.sig == sig)
            .map(|s| s.writing_app.clone())
    }

    /// Read the stamp of `path`, known to be at `sig`, unless it is cached.
    /// The header read happens outside the lock. True if it read the file.
    pub fn refresh_stamp(&self, path: &Path, sig: FileSig) -> bool {
        if self.cached_stamp(path, sig).is_some() {
            return false;
        }
        let app = match read_stamp(path) {
            Ok(app) => app,
            // The storage failed, not the file: cache nothing, retry next pass.
            Err(_) => return false,
        };
        self.lock_stamps().insert(
            path.to_path_buf(),
            Stamp {
                sig,
                writing_app: app,
            },
        );
        true
    }

    /// Drop the cached audit of `path`, so the auditor reads it again.
    pub fn forget_audit(&self, path: &Path) -> bool {
        self.lock_audits().remove(path).is_some()
    }

    /// Drop every cached audit. Returns how many there were.
    pub fn forget_all_audits(&self) -> usize {
        let mut a = self.lock_audits();
        let n = a.len();
        a.clear();
        n
    }

    /// Record a known stamp for `path` at `sig` (a remux just wrote it).
    pub fn record_at(&self, path: &Path, sig: FileSig, writing_app: Option<String>) {
        self.lock_stamps()
            .insert(path.to_path_buf(), Stamp { sig, writing_app });
    }

    /// Record what a finished remux wrote, against the file as it is now.
    pub fn record(&self, path: &Path, writing_app: Option<String>) {
        let Some(sig) = FileSig::stat(path) else {
            self.lock_stamps().remove(path);
            return;
        };
        self.lock_stamps()
            .insert(path.to_path_buf(), Stamp { sig, writing_app });
    }

    /// The cached audit of `path`, if it still matches the file.
    pub fn audit(&self, path: &Path, sig: FileSig) -> Option<AuditReport> {
        self.lock_audits()
            .get(path)
            .filter(|(s, _)| *s == sig)
            .map(|(_, r)| r.clone())
    }

    /// Audit `path` unless the cached result still matches it. True if it ran.
    pub fn refresh_audit(&self, path: &Path) -> bool {
        let Some(sig) = FileSig::stat(path) else {
            return false;
        };
        self.refresh_audit_at(path, sig)
    }

    /// [`Self::refresh_audit`] for a file already known to be at `sig`.
    pub fn refresh_audit_at(&self, path: &Path, sig: FileSig) -> bool {
        if self.audit(path, sig).is_some() {
            return false;
        }
        let Some(report) = audit_fast(path) else {
            return false;
        };
        self.lock_audits().insert(path.to_path_buf(), (sig, report));
        true
    }

    fn lock_stamps(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Stamp>> {
        self.stamps.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_audits(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, (FileSig, AuditReport)>> {
        self.audits.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A hand-built MKV for tests: EBML header, Segment, Info, tracks, optional Cues.
#[cfg(test)]
pub(crate) mod testmkv {
    fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.push(0x01);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    pub(crate) fn mkv(
        app: &str,
        duration_secs: Option<f64>,
        cue_secs: Option<u64>,
        video: bool,
    ) -> Vec<u8> {
        let mut info = el(&[0x2A, 0xD7, 0xB1], &1_000_000u64.to_be_bytes());
        if let Some(d) = duration_secs {
            info.extend(el(&[0x44, 0x89], &(d * 1000.0).to_be_bytes()));
        }
        info.extend(el(&[0x4D, 0x80], app.as_bytes()));
        info.extend(el(&[0x57, 0x41], app.as_bytes()));
        let mut entry = el(&[0xD7], &[1]);
        entry.extend(el(&[0x83], &[if video { 1 } else { 2 }]));
        entry.extend(el(&[0x86], b"V_MPEG4/ISO/AVC"));
        let mut body = el(&[0x15, 0x49, 0xA9, 0x66], &info);
        body.extend(el(&[0x16, 0x54, 0xAE, 0x6B], &el(&[0xAE], &entry)));
        if let Some(t) = cue_secs {
            let point = el(&[0xB3], &(t * 1000).to_be_bytes());
            body.extend(el(&[0x1C, 0x53, 0xBB, 0x6B], &el(&[0xBB], &point)));
        }
        let mut out = el(&[0x1A, 0x45, 0xDF, 0xA3], &[]);
        out.extend(el(&[0x18, 0x53, 0x80, 0x67], &body));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testmkv::mkv;
    use super::*;

    const RUNNING: (u32, u32, u32) = (1, 8, 0);

    #[test]
    fn muxed_with_compares_against_the_running_version() {
        let m = |a: Option<&str>| MuxedWith::from_app(a, RUNNING);
        assert_eq!(
            m(Some("freemkv 1.8.0 (gabc1234)")),
            MuxedWith::Current {
                version: "1.8.0".into()
            }
        );
        assert!(m(Some("freemkv 1.6.11 (g0)")).out_of_date());
        assert!(m(Some("freemkv 1.9.0")).out_of_date(), "any other version");
        assert!(m(Some("libmakemkv v1.17.5")).out_of_date());
        assert!(
            m(Some("freemkv")).out_of_date(),
            "the unversioned stamp older builds wrote"
        );
        assert_eq!(m(None), MuxedWith::Unknown);
        assert!(!m(Some("  ")).out_of_date());
    }

    #[test]
    fn a_storage_error_is_not_cached_as_a_verdict() {
        let t = tempfile::tempdir().unwrap();
        // A directory opens but cannot be read: an I/O failure, not a file verdict.
        let p = t.path().join("odd.mkv");
        std::fs::create_dir(&p).unwrap();
        assert!(audit_fast(&p).is_none());
        let sig = FileSig::stat(&p).unwrap();
        let cache = ProbeCache::default();
        assert!(!cache.refresh_audit_at(&p, sig), "nothing cached");
        assert!(cache.audit(&p, sig).is_none());
        assert!(!cache.refresh_stamp(&p, sig));
        assert_eq!(cache.cached_stamp(&p, sig), None, "retried next pass");
        // A missing file is not a verdict either.
        assert!(audit_fast(&t.path().join("gone.mkv")).is_none());
        // A short file is: it really is not an MKV.
        let short = t.path().join("short.mkv");
        std::fs::write(&short, b"ab").unwrap();
        assert_eq!(audit_fast(&short).unwrap().issues, [AuditIssue::NotMkv]);
    }

    #[test]
    fn stamps_shorten_to_program_and_version() {
        assert_eq!(
            short_label("mkvmerge v96.0 ('It's My Life') 64-bit"),
            "mkvmerge 96.0"
        );
        assert_eq!(short_label("freemkv 1.7.7 (gc8e67f1)"), "freemkv 1.7.7");
        assert_eq!(
            short_label("MakeMKV v1.17.5 linux(x64-release)"),
            "MakeMKV 1.17.5"
        );
        assert_eq!(short_label("freemkv"), "freemkv");
        assert_eq!(short_label(""), "");
    }

    #[test]
    fn the_library_versions_classify_as_the_user_expects() {
        // The running 1.7.7 build against what the test library was written with.
        let m = |a: &str| MuxedWith::from_app(Some(a), (1, 7, 7));
        assert!(!m("freemkv 1.7.7 (gc8e67f1)").out_of_date());
        assert!(m("freemkv 1.6.11 (g1234567)").out_of_date());
        assert!(m("mkvmerge v96.0 ('It's My Life') 64-bit").out_of_date());
    }

    #[test]
    fn cached_lookups_never_touch_the_disk() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.mkv");
        std::fs::write(&p, mkv("freemkv 1.6.11 (g1)", Some(60.0), Some(55), true)).unwrap();
        let sig = FileSig::stat(&p).unwrap();
        let cache = ProbeCache::default();
        assert_eq!(cache.cached_stamp(&p, sig), None, "not read yet");
        assert!(cache.refresh_stamp(&p, sig));
        assert!(!cache.refresh_stamp(&p, sig), "cached");
        std::fs::remove_file(&p).unwrap();
        assert_eq!(
            cache.cached_stamp(&p, sig),
            Some(Some("freemkv 1.6.11 (g1)".into()))
        );
    }

    #[test]
    fn the_running_version_is_the_crate_version() {
        let v = running_version();
        assert_eq!(
            format!("{}.{}.{}", v.0, v.1, v.2),
            env!("CARGO_PKG_VERSION").split('-').next().unwrap()
        );
    }

    #[test]
    fn a_changed_file_is_re_read_without_anyone_asking() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.mkv");
        std::fs::write(&p, mkv("freemkv 1.6.11 (g1)", Some(60.0), Some(55), true)).unwrap();
        let cache = ProbeCache::default();
        assert_eq!(
            cache.writing_app(&p),
            Some(Some("freemkv 1.6.11 (g1)".into()))
        );
        // A longer stamp changes the size, so the cached entry no longer matches.
        std::fs::write(
            &p,
            mkv("freemkv 1.8.0 (gnewer)", Some(60.0), Some(55), true),
        )
        .unwrap();
        assert_eq!(
            cache.writing_app(&p),
            Some(Some("freemkv 1.8.0 (gnewer)".into()))
        );
        assert_eq!(cache.writing_app(&t.path().join("gone.mkv")), None);
    }

    #[test]
    fn the_fast_audit_checks_structure() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.mkv");
        let audit = |bytes: Vec<u8>| {
            std::fs::write(&p, bytes).unwrap();
            audit_fast(&p).unwrap()
        };
        let good = audit(mkv("freemkv 1.8.0", Some(7200.0), Some(7195), true));
        assert!(good.ok, "{good:?}");
        assert_eq!(good.video_tracks, 1);
        assert_eq!(good.video, ["AVC"]);
        assert_eq!(good.runtime_secs, Some(7195.0));

        let short = audit(mkv("freemkv 1.8.0", Some(7200.0), Some(2400), true));
        assert!(!short.ok);
        assert!(matches!(
            short.issues[0],
            AuditIssue::RuntimeMismatch { .. }
        ));

        let no_cues = audit(mkv("freemkv 1.8.0", Some(60.0), None, true));
        assert!(no_cues.ok, "a missing index only warns");
        assert_eq!(no_cues.issues, [AuditIssue::NoCues]);

        let audio_only = audit(mkv("x", Some(60.0), Some(58), false));
        assert_eq!(audio_only.issues, [AuditIssue::NoVideo]);

        let no_duration = audit(mkv("x", None, Some(58), true));
        assert_eq!(no_duration.issues, [AuditIssue::NoDuration]);

        assert_eq!(audit(vec![0x47; 4096]).issues, [AuditIssue::NotMkv]);
        let mut torn = mkv("x", Some(60.0), Some(58), true);
        torn.truncate(20);
        assert!(matches!(
            audit(torn).issues[..],
            [AuditIssue::Unreadable { .. }]
        ));
    }

    #[test]
    fn an_audit_is_redone_only_when_the_file_changes() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.mkv");
        std::fs::write(&p, mkv("x", Some(60.0), Some(58), true)).unwrap();
        let cache = ProbeCache::default();
        assert!(cache.refresh_audit(&p));
        assert!(!cache.refresh_audit(&p));
        std::fs::write(
            &p,
            [mkv("x", Some(60.0), Some(5), true), vec![0; 7]].concat(),
        )
        .unwrap();
        assert!(cache.refresh_audit(&p));
        let sig = FileSig::stat(&p).unwrap();
        assert!(!cache.audit(&p, sig).unwrap().ok);
    }
}
