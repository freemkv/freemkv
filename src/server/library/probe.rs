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

/// Which audit ran: the fast structural pass (the full decode is [`super::deep`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDepth {
    Fast,
}

/// One finding of an audit.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// What each track carries; `None` in a report stored before it was read, until the
    /// quick lane fills it in.
    #[serde(default)]
    pub detail: Option<super::media::MediaDetail>,
}

impl AuditReport {
    /// The stored report predates the current media reader.
    pub fn needs_detail(&self) -> bool {
        self.detail
            .as_ref()
            .is_none_or(|d| d.version < super::media::VERSION)
    }
}

/// One audio track as the Library shows it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
        detail: Some(super::media::empty()),
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
    // A storage failure here leaves the detail to a later pass; the verdict stands.
    report.detail = super::media::read(path);
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

/// The writing-app stamps. Filesystem work never happens under its lock.
#[derive(Default)]
pub struct ProbeCache {
    stamps: Mutex<HashMap<PathBuf, Stamp>>,
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

    /// Forget the stamps of every path not in `keep`.
    pub fn retain(&self, keep: &[(PathBuf, FileSig)]) {
        let keep: std::collections::HashSet<&Path> =
            keep.iter().map(|(p, _)| p.as_path()).collect();
        self.lock_stamps().retain(|p, _| keep.contains(p.as_path()));
    }

    fn lock_stamps(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Stamp>> {
        self.stamps.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A hand-built MKV for tests: EBML header, Segment, Info, tracks, optional Cues.
#[cfg(test)]
#[path = "probe_testmkv_tests.rs"]
pub(crate) mod testmkv;

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
