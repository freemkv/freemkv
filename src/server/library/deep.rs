//! Deep audit: an ffmpeg full decode of every MKV, one file at a time in the background,
//! behind the `deep_audit` setting. The fast audit reads structure; this finds damage
//! only decoding shows (a corrupt slice payload plays as garbage but parses fine).
//!
//! Two stages, cheapest first, as the mkv-audit prototype ran them: demux every stream
//! (packet integrity for the whole file, Dolby Vision layer and subtitles included),
//! then decode the base video and every audio track. A decode that eats memory without
//! bound is killed and judged; a timeout, a signal or a dropped share is inconclusive
//! and retried with backoff. Verdicts persist in `deep-audit.json`.

use super::probe::FileSig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FILE: &str = "deep-audit.json";
// A normal decode stays near 1 GB; some HEVC streams grow without bound (one took a host down).
const RSS_CAP_MIB: u64 = 3072;
const TIMEOUT: Duration = Duration::from_secs(3 * 3600);
const RETRY_BASE_SECS: u64 = 600;
const RETRY_CAP_SECS: u64 = 6 * 3600;
const STDERR_TAIL_BYTES: u64 = 2_000_000;

/// One ffmpeg run's outcome, judged. Carried whole to the details dialog.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub clean: bool,
    /// False when the run says nothing about the file (timeout, killed, share down).
    pub completed: bool,
    pub reason: String,
    pub errors: u64,
    pub stage: String,
    pub rc: Option<i32>,
    pub signal: Option<String>,
    pub peak_rss_mib: u64,
    /// ffmpeg cannot decode a track (a codec gap, not damage).
    pub limited: bool,
    pub flood: u64,
    pub bad: Vec<String>,
    pub stderr: Vec<String>,
    pub stderr_lines: usize,
    pub sample: Vec<String>,
    pub scanned: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryState {
    Done,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    size: u64,
    mtime_ns: i128,
    state: EntryState,
    verdict: Verdict,
    attempts: u32,
    last_try: u64,
}

impl Entry {
    fn matches(&self, sig: FileSig) -> bool {
        self.size == sig.size && self.mtime_ns == sig.mtime_ns
    }
}

/// A row's deep-audit state as the Library shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeepView {
    /// `clean` | `corrupt` | `scanning` | `aborted` | `pending`.
    pub state: &'static str,
    pub verdict: Option<Verdict>,
}

/// The persisted verdicts and the file being decoded now.
pub struct Store {
    path: PathBuf,
    entries: Mutex<HashMap<PathBuf, Entry>>,
    scanning: Mutex<Option<PathBuf>>,
}

impl Store {
    pub fn open(config_dir: &Path) -> Self {
        let path = config_dir.join(FILE);
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            entries: Mutex::new(entries),
            scanning: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, entries: &HashMap<PathBuf, Entry>) {
        let Ok(json) = serde_json::to_vec(entries) else {
            return;
        };
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// What the Library shows for `path` at `sig`; `None` when deep audit is off.
    pub fn view(&self, path: &Path, sig: FileSig, enabled: bool) -> Option<DeepView> {
        let scanning = self.scanning.lock().unwrap_or_else(|e| e.into_inner());
        if scanning.as_deref() == Some(path) {
            return Some(DeepView {
                state: "scanning",
                verdict: None,
            });
        }
        drop(scanning);
        let e = self.lock().get(path).filter(|e| e.matches(sig)).cloned();
        match e {
            Some(e) if e.state == EntryState::Done => Some(DeepView {
                state: if e.verdict.clean { "clean" } else { "corrupt" },
                verdict: Some(e.verdict),
            }),
            _ if !enabled => None,
            Some(e) => Some(DeepView {
                state: "aborted",
                verdict: Some(e.verdict),
            }),
            None => Some(DeepView {
                state: "pending",
                verdict: None,
            }),
        }
    }

    /// Whether `path` at `sig` is due a decode at `now`.
    fn due(&self, path: &Path, sig: FileSig, now: u64) -> bool {
        match self.lock().get(path) {
            None => true,
            Some(e) if !e.matches(sig) => true,
            Some(e) if e.state == EntryState::Done => false,
            Some(e) => retry_due(e.attempts, e.last_try, now),
        }
    }

    /// Forget the verdicts of `paths`, so they decode again. Returns how many.
    pub fn forget(&self, paths: &[PathBuf]) -> usize {
        let mut e = self.lock();
        let n = paths.iter().filter(|p| e.remove(*p).is_some()).count();
        if n > 0 {
            self.save(&e);
        }
        n
    }

    // Record a finished run, unless the file changed under it.
    fn record(&self, path: &Path, sig: FileSig, v: Verdict, now: u64) {
        if FileSig::stat(path) != Some(sig) {
            return;
        }
        let mut e = self.lock();
        let attempts = match e.get(path) {
            Some(prev) if prev.matches(sig) && prev.state == EntryState::Error => prev.attempts,
            _ => 0,
        };
        let entry = if v.completed {
            Entry {
                size: sig.size,
                mtime_ns: sig.mtime_ns,
                state: EntryState::Done,
                verdict: v,
                attempts: 0,
                last_try: now,
            }
        } else {
            Entry {
                size: sig.size,
                mtime_ns: sig.mtime_ns,
                state: EntryState::Error,
                verdict: v,
                attempts: attempts + 1,
                last_try: now,
            }
        };
        e.insert(path.to_path_buf(), entry);
        self.save(&e);
    }
}

// A bailed decode retries after 10 min, doubling per attempt, capped at 6 h.
fn retry_due(attempts: u32, last_try: u64, now: u64) -> bool {
    if attempts == 0 {
        return true;
    }
    let delay = RETRY_BASE_SECS
        .saturating_mul(1u64 << (attempts - 1).min(20))
        .min(RETRY_CAP_SECS);
    now.saturating_sub(last_try) >= delay
}

// Decoder errors that mean damage.
fn is_bad(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    const PLAIN: [&str; 5] = [
        "error while decoding",
        "invalid data found",
        "non-existing pps",
        "decode_slice_header error",
        "could not find ref",
    ];
    if PLAIN.iter().any(|p| l.contains(p)) {
        return true;
    }
    if l.match_indices("concealing ")
        .any(|(i, m)| l[i + m.len()..].starts_with(|c: char| c.is_ascii_digit()))
    {
        return true;
    }
    ["corrupt ", "corrupted "].iter().any(|c| {
        ["frame", "macroblock", "data"]
            .iter()
            .any(|w| l.contains(&format!("{c}{w}")))
    })
}

// Entropy-decode desync: ffmpeg logs these and still exits 0, yet a clean file logs none.
fn flood_count(line: &str) -> u64 {
    let l = line.to_ascii_lowercase();
    let qp = l
        .find("cu_qp_delta ")
        .is_some_and(|i| l[i..].contains("outside the valid range"));
    u64::from(qp)
        + l.matches("cabac_max_bin").count() as u64
        + l.matches("skipping invalid undecodable nalu").count() as u64
}

// A codec feature ffmpeg can't decode; the stream itself is fine.
fn is_limitation(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    [
        "not supported",
        "not yet implemented",
        "patches welcome",
        "deficit samples",
    ]
    .iter()
    .any(|p| l.contains(p))
}

// Lines a decoder limitation causes downstream; damage only without one.
fn is_ambiguous(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("invalid data found") || l.contains("error submitting packet")
}

/// What one ffmpeg run did.
#[derive(Clone, Debug, Default)]
pub struct Run {
    pub rc: Option<i32>,
    pub signal: Option<i32>,
    pub lines: Vec<String>,
    pub peak_rss_mib: u64,
    pub runaway: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub flood: u64,
}

/// Judge one run. `stage` is `demux` or `decode`.
pub fn classify(run: &Run, stage: &str, now: u64) -> Verdict {
    let limited = run.lines.iter().any(|l| is_limitation(l));
    let bad: Vec<String> = run
        .lines
        .iter()
        .filter(|l| is_bad(l) && !(limited && is_ambiguous(l)))
        .cloned()
        .collect();
    let flood_corrupt = run.flood > 0;
    let signal = run.signal.map(signal_name);
    let (completed, clean, reason) = if run.runaway {
        (true, false, "memory_runaway")
    } else if run.timed_out {
        (false, false, "timeout")
    } else if run.signal.is_some() {
        let oom = run.signal == Some(libc::SIGKILL);
        (false, false, if oom { "oom" } else { "killed" })
    } else {
        let rc = run.rc.unwrap_or(-1);
        let clean = bad.is_empty() && !flood_corrupt && (rc == 0 || limited);
        let reason = if !bad.is_empty() {
            if stage == "demux" {
                "demux_errors"
            } else {
                "decode_errors"
            }
        } else if flood_corrupt {
            "bitstream_corruption"
        } else if limited {
            "decoder_limitation"
        } else if rc != 0 {
            "nonzero_exit"
        } else {
            "clean"
        };
        (true, clean, reason)
    };
    let errors = if !bad.is_empty() {
        bad.len() as u64
    } else if flood_corrupt {
        run.flood
    } else if completed && !clean {
        1
    } else {
        0
    };
    let tail = run.lines.len().saturating_sub(200);
    Verdict {
        clean,
        completed,
        reason: reason.into(),
        errors,
        stage: stage.into(),
        rc: run.rc,
        signal,
        peak_rss_mib: run.peak_rss_mib,
        limited,
        flood: run.flood,
        bad: bad.into_iter().take(50).collect(),
        stderr: run.lines[tail..].to_vec(),
        stderr_lines: run.lines.len(),
        sample: run.lines.iter().take(8).cloned().collect(),
        scanned: now,
    }
}

fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGSEGV => "SIGSEGV".into(),
        libc::SIGABRT => "SIGABRT".into(),
        s => format!("signal {s}"),
    }
}

#[cfg(target_os = "linux")]
fn rss_mib(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

#[cfg(not(target_os = "linux"))]
fn rss_mib(_pid: u32) -> u64 {
    0
}

/// Run `argv` at low CPU and IO priority, stderr to `err_file`, under the memory cap and
/// timeout; `stop` ends it early (setting off, a remux or rip wants the disks).
pub fn run_monitored(argv: &[&str], err_file: &Path, stop: &dyn Fn() -> bool) -> Run {
    let mut run = Run::default();
    let Ok(err) = std::fs::File::create(err_file) else {
        run.rc = Some(-1);
        run.lines = vec![format!("cannot create {}", err_file.display())];
        return run;
    };
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::nice(15);
                #[cfg(target_os = "linux")]
                libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
                Ok(())
            });
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            run.rc = Some(-1);
            run.lines = vec![format!("cannot run {}: {e}", argv[0])];
            return run;
        }
    };
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => break None,
        }
        let rss = rss_mib(child.id());
        run.peak_rss_mib = run.peak_rss_mib.max(rss);
        if rss > RSS_CAP_MIB {
            run.runaway = true;
        } else if started.elapsed() > TIMEOUT {
            run.timed_out = true;
        } else if stop() {
            run.cancelled = true;
        } else {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let _ = child.kill();
        break child.wait().ok();
    };
    if let Some(s) = status {
        run.rc = s.code();
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            run.signal = s.signal();
        }
    }
    if run.runaway || run.timed_out || run.cancelled {
        run.signal = None;
    }
    read_stderr(err_file, &mut run);
    let _ = std::fs::remove_file(err_file);
    run
}

// Count desync lines across all of stderr; keep only its last 2 MB as lines.
fn read_stderr(path: &Path, run: &mut Run) {
    let Ok(f) = std::fs::File::open(path) else {
        return;
    };
    let mut r = std::io::BufReader::new(f);
    let mut buf = Vec::new();
    while r.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
        run.flood += flood_count(&String::from_utf8_lossy(&buf));
        buf.clear();
    }
    let mut f = r.into_inner();
    let len = f.seek(SeekFrom::End(0)).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(STDERR_TAIL_BYTES)));
    let mut tail = Vec::new();
    let _ = f.read_to_end(&mut tail);
    run.lines = String::from_utf8_lossy(&tail)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
}

/// The full audit of one MKV: demux every stream, then decode base video and audio.
/// `None` when stopped early. A failure while the file or library is unreachable is
/// the share, not the file.
pub fn full_decode(
    ffmpeg: &Path,
    mkv: &Path,
    library: &Path,
    err_file: &Path,
    stop: &dyn Fn() -> bool,
) -> Option<Verdict> {
    let bin = ffmpeg.to_string_lossy();
    let file = mkv.to_string_lossy();
    let head = [&*bin, "-nostdin", "-v", "error", "-i", &*file];
    let guard = |v: Verdict| {
        if !v.clean && (std::fs::metadata(mkv).is_err() || std::fs::read_dir(library).is_err()) {
            return Verdict {
                completed: false,
                reason: "media_unavailable".into(),
                errors: 0,
                ..v
            };
        }
        v
    };
    let demux: Vec<&str> = head
        .iter()
        .copied()
        .chain(["-map", "0", "-c", "copy", "-f", "null", "-"])
        .collect();
    let run = run_monitored(&demux, err_file, stop);
    if run.cancelled {
        return None;
    }
    let v = guard(classify(&run, "demux", crate::server::util::epoch_secs()));
    if !v.clean {
        return Some(v);
    }
    let peak = run.peak_rss_mib;
    let decode: Vec<&str> = head
        .iter()
        .copied()
        .chain(["-map", "0:v:0", "-map", "0:a?", "-f", "null", "-"])
        .collect();
    let mut run = run_monitored(&decode, err_file, stop);
    if run.cancelled {
        return None;
    }
    run.peak_rss_mib = run.peak_rss_mib.max(peak);
    Some(guard(classify(
        &run,
        "decode",
        crate::server::util::epoch_secs(),
    )))
}

/// The ffmpeg the image bundles, if this install has one.
pub fn ffmpeg() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("ffmpeg"))
        .find(|p| p.is_file())
}

/// Pick the next file to decode: oldest first, audited fast, not already judged.
pub fn next_target(
    store: &Store,
    files: &[(PathBuf, FileSig)],
    now: u64,
) -> Option<(PathBuf, FileSig)> {
    let mut due: Vec<&(PathBuf, FileSig)> = files
        .iter()
        .filter(|(p, s)| store.due(p, *s, now))
        .collect();
    due.sort_by_key(|(_, s)| s.mtime_ns);
    due.first().map(|(p, s)| (p.clone(), *s))
}

/// Decode `path` and record the verdict; false when stopped early.
pub fn audit_one(
    store: &Store,
    ffmpeg: &Path,
    path: &Path,
    sig: FileSig,
    library: &Path,
    err_file: &Path,
    stop: &dyn Fn() -> bool,
) -> bool {
    *store.scanning.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
    let v = full_decode(ffmpeg, path, library, err_file, stop);
    *store.scanning.lock().unwrap_or_else(|e| e.into_inner()) = None;
    match v {
        Some(v) => {
            store.record(path, sig, v, crate::server::util::epoch_secs());
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(rc: i32, lines: &[&str]) -> Run {
        Run {
            rc: Some(rc),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_quiet_run_is_clean() {
        let v = classify(&run(0, &[]), "decode", 1);
        assert!(v.clean && v.completed);
        assert_eq!(v.reason, "clean");
    }

    #[test]
    fn decoder_errors_are_corruption_named_by_stage() {
        let lines = [
            "[hevc @ 0x1] error while decoding MB 3 4",
            "concealing 120 DC errors",
        ];
        let v = classify(&run(0, &lines), "decode", 1);
        assert!(!v.clean && v.completed);
        assert_eq!((v.reason.as_str(), v.errors), ("decode_errors", 2));
        assert_eq!(classify(&run(1, &lines), "demux", 1).reason, "demux_errors");
        assert!(!is_bad("concealing errors"), "needs a count after it");
    }

    #[test]
    fn a_desync_flood_is_corruption_even_at_exit_zero() {
        let mut r = run(0, &[]);
        r.flood = 740;
        let v = classify(&r, "decode", 1);
        assert_eq!(
            (v.clean, v.reason.as_str(), v.errors),
            (false, "bitstream_corruption", 740)
        );
        assert_eq!(flood_count("cu_qp_delta 52 is outside the valid range"), 1);
        assert_eq!(flood_count("CABAC_MAX_BIN : 32"), 1);
    }

    #[test]
    fn a_decoder_limitation_excuses_only_the_ambiguous_lines() {
        let lines = [
            "[dca @ 0x1] Deficit samples are not supported",
            "Error submitting packet to decoder: Invalid data found when processing input",
        ];
        let v = classify(&run(1, &lines), "decode", 1);
        assert!(v.clean && v.limited, "{v:?}");
        assert_eq!(v.reason, "decoder_limitation");
        let with_real = [lines[0], lines[1], "error while decoding MB 1 1"];
        assert!(!classify(&run(1, &with_real), "decode", 1).clean);
    }

    #[test]
    fn runaway_is_a_verdict_but_timeout_and_signals_retry() {
        let mut r = run(0, &[]);
        r.runaway = true;
        let v = classify(&r, "decode", 1);
        assert!(v.completed && !v.clean && v.reason == "memory_runaway");
        let mut r = run(0, &[]);
        r.timed_out = true;
        assert!(!classify(&r, "decode", 1).completed);
        let r = Run {
            signal: Some(libc::SIGKILL),
            ..Default::default()
        };
        let v = classify(&r, "decode", 1);
        assert_eq!((v.completed, v.reason.as_str()), (false, "oom"));
    }

    #[test]
    fn retries_back_off_and_cap() {
        assert!(retry_due(0, 100, 100));
        assert!(!retry_due(1, 0, 599));
        assert!(retry_due(1, 0, 600));
        assert!(!retry_due(2, 0, 1199));
        assert!(retry_due(30, 0, RETRY_CAP_SECS));
    }

    fn sig_of(p: &Path) -> FileSig {
        FileSig::stat(p).unwrap()
    }

    #[test]
    fn verdicts_persist_and_follow_the_file() {
        let t = tempfile::tempdir().unwrap();
        let mkv = t.path().join("A.mkv");
        std::fs::write(&mkv, b"one").unwrap();
        let sig = sig_of(&mkv);
        let store = Store::open(t.path());
        assert_eq!(store.view(&mkv, sig, true).unwrap().state, "pending");
        assert!(
            store.view(&mkv, sig, false).is_none(),
            "off shows nothing new"
        );
        store.record(&mkv, sig, classify(&run(0, &[]), "decode", 1), 1);
        let reopened = Store::open(t.path());
        assert_eq!(reopened.view(&mkv, sig, false).unwrap().state, "clean");
        assert!(!reopened.due(&mkv, sig, 2));
        std::fs::write(&mkv, b"changed").unwrap();
        let moved = sig_of(&mkv);
        assert!(reopened.due(&mkv, moved, 2), "a changed file decodes again");
        assert_eq!(reopened.forget(std::slice::from_ref(&mkv)), 1);
        assert!(reopened.due(&mkv, sig, 2));
    }

    #[test]
    fn an_inconclusive_run_backs_off() {
        let t = tempfile::tempdir().unwrap();
        let mkv = t.path().join("A.mkv");
        std::fs::write(&mkv, b"x").unwrap();
        let sig = sig_of(&mkv);
        let store = Store::open(t.path());
        let mut r = run(0, &[]);
        r.timed_out = true;
        store.record(&mkv, sig, classify(&r, "decode", 1), 1000);
        assert_eq!(store.view(&mkv, sig, true).unwrap().state, "aborted");
        assert!(!store.due(&mkv, sig, 1000 + 599));
        assert!(store.due(&mkv, sig, 1000 + 600));
    }

    #[cfg(unix)]
    #[test]
    fn the_runner_keeps_stderr_and_counts_floods() {
        let t = tempfile::tempdir().unwrap();
        let err = t.path().join("err");
        let r = run_monitored(
            &[
                "sh",
                "-c",
                "echo 'CABAC_MAX_BIN : 32' >&2; echo 'error while decoding' >&2; exit 3",
            ],
            &err,
            &|| false,
        );
        assert_eq!((r.rc, r.flood, r.lines.len()), (Some(3), 1, 2));
        assert!(!err.exists());
        let stopped = run_monitored(&["sh", "-c", "sleep 30"], &err, &|| true);
        assert!(stopped.cancelled);
    }
}
