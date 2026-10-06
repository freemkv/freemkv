//! Deep audit: an ffmpeg full decode of every MKV, one file at a time in the background,
//! behind the `deep_audit` setting. The fast audit reads structure; this finds damage
//! only decoding shows (a corrupt slice payload plays as garbage but parses fine).
//!
//! Two stages, cheapest first, as the mkv-audit prototype ran them: demux every stream
//! (packet integrity for the whole file, Dolby Vision layer and subtitles included),
//! then decode the base video and every audio track. A decode that eats memory without
//! bound is killed and judged; a timeout, a signal or a dropped share is inconclusive
//! and retried with backoff. [`super::audit`] queues the files and keeps the verdicts.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// A normal decode stays near 1 GB; some HEVC streams grow without bound (one took a host down).
const RSS_CAP_MIB: u64 = 3072;
const TIMEOUT: Duration = Duration::from_secs(3 * 3600);
const RETRY_BASE_SECS: u64 = 600;
const RETRY_CAP_SECS: u64 = 6 * 3600;
const STDERR_TAIL_BYTES: u64 = 2_000_000;
// ffmpeg is stopped and the file judged once its stderr passes this.
const STDERR_CAP_BYTES: u64 = 256 << 20;

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
    /// How far into the movie the decode got, in seconds.
    #[serde(default)]
    pub reached_secs: Option<f64>,
    /// Where a damaged file fails, sampled after the verdict; `None` before this existed.
    #[serde(default)]
    pub forensic: Option<Forensic>,
}

/// Short decode and demux windows spread over a damaged file: where it fails, and in
/// which layer. Buckets: `container_framing` (packets fail to copy: a muxer fault),
/// `payload_bitstream` (frames decode damaged while copying clean: usually the source),
/// `timestamp` (non-monotonic timestamps: a muxer fault), `inconclusive` (no window hit).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Forensic {
    pub window_secs: u32,
    pub windows: Vec<Window>,
    pub buckets: Vec<String>,
    pub pct_windows_corrupt: u32,
    pub container_sample: Vec<String>,
    pub timestamp_sample: Vec<String>,
}

/// One sampled window: where it starts and what each pass found there.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    pub at_secs: u64,
    pub payload_errors: u64,
    pub container_errors: u64,
    pub timestamp_errors: u64,
}

const FORENSIC_WINDOWS: u64 = 12;
const FORENSIC_WINDOW_SECS: u32 = 5;

// A bailed decode retries after 10 min, doubling per attempt, capped at 6 h.
pub(super) fn retry_due(attempts: u32, last_try: u64, now: u64) -> bool {
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
    /// Its stderr outgrew the cap: a desync flood no clean file produces.
    pub overflow: bool,
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
    } else if run.overflow {
        (true, false, "stderr_overflow")
    } else if run.timed_out {
        (false, false, "timeout")
    } else if run.signal.is_some() {
        let oom = run.signal == Some(libc::SIGKILL);
        (false, false, if oom { "oom" } else { "killed" })
    } else if let Some(rc) = run.rc {
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
    } else {
        // No exit status: ffmpeg never ran, or could not be waited on. Says nothing of the file.
        (false, false, "not_run")
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
        reached_secs: None,
        forensic: None,
    }
}

fn is_timestamp_fault(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("non monoton") || l.contains("non-monoton")
}

/// Sample a damaged file in [`FORENSIC_WINDOWS`] windows, each decoded (the base video)
/// and demuxed (every stream copied). `None` when stopped.
pub fn forensic(
    ffmpeg: &Path,
    mkv: &Path,
    duration: f64,
    err_file: &Path,
    stop: &dyn Fn() -> bool,
) -> Option<Forensic> {
    let bin = ffmpeg.to_string_lossy();
    let file = mkv.to_string_lossy();
    let len = FORENSIC_WINDOW_SECS.to_string();
    let mut f = Forensic {
        window_secs: FORENSIC_WINDOW_SECS,
        ..Forensic::default()
    };
    for i in 0..FORENSIC_WINDOWS {
        let at = (duration.max(0.0) * (i as f64 + 0.5) / FORENSIC_WINDOWS as f64) as u64;
        let ss = at.to_string();
        let head = [
            &*bin, "-nostdin", "-v", "warning", "-ss", &ss, "-i", &*file, "-t", &len,
        ];
        let decode: Vec<&str> = head
            .iter()
            .copied()
            .chain(["-map", "0:v:0", "-f", "null", "-"])
            .collect();
        let run = run_monitored(&decode, err_file, stop, &|_| {});
        if run.cancelled {
            return None;
        }
        let payload = run.lines.iter().filter(|l| is_bad(l)).count() as u64 + run.flood;
        let demux: Vec<&str> = head
            .iter()
            .copied()
            .chain(["-map", "0", "-c", "copy", "-f", "null", "-"])
            .collect();
        let run = run_monitored(&demux, err_file, stop, &|_| {});
        if run.cancelled {
            return None;
        }
        let container: Vec<&String> = run.lines.iter().filter(|l| is_bad(l)).collect();
        let stamps: Vec<&String> = run.lines.iter().filter(|l| is_timestamp_fault(l)).collect();
        f.container_sample.extend(
            container
                .iter()
                .take(20 - f.container_sample.len().min(20))
                .map(|l| (*l).clone()),
        );
        f.timestamp_sample.extend(
            stamps
                .iter()
                .take(10 - f.timestamp_sample.len().min(10))
                .map(|l| (*l).clone()),
        );
        f.windows.push(Window {
            at_secs: at,
            payload_errors: payload,
            container_errors: container.len() as u64,
            timestamp_errors: stamps.len() as u64,
        });
    }
    let hit = |g: fn(&Window) -> u64| f.windows.iter().any(|w| g(w) > 0);
    for (bucket, found) in [
        ("container_framing", hit(|w| w.container_errors)),
        ("payload_bitstream", hit(|w| w.payload_errors)),
        ("timestamp", hit(|w| w.timestamp_errors)),
    ] {
        if found {
            f.buckets.push(bucket.into());
        }
    }
    if f.buckets.is_empty() {
        f.buckets.push("inconclusive".into());
    }
    let bad = f.windows.iter().filter(|w| w.payload_errors > 0).count();
    f.pct_windows_corrupt = (100 * bad / f.windows.len().max(1)) as u32;
    Some(f)
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
/// timeout; `stop` ends it early (setting off, a remux or rip wants the disks). Each
/// `out_time_us=` line of the `-progress pipe:1` stream reaches `on_secs`.
pub fn run_monitored(
    argv: &[&str],
    err_file: &Path,
    stop: &dyn Fn() -> bool,
    on_secs: &(dyn Fn(f64) + Sync),
) -> Run {
    run_capped(argv, err_file, STDERR_CAP_BYTES, stop, on_secs)
}

fn run_capped(
    argv: &[&str],
    err_file: &Path,
    stderr_cap: u64,
    stop: &dyn Fn() -> bool,
    on_secs: &(dyn Fn(f64) + Sync),
) -> Run {
    let mut run = Run::default();
    let Ok(err) = std::fs::File::create(err_file) else {
        run.lines = vec![format!("cannot create {}", err_file.display())];
        return run;
    };
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
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
            run.lines = vec![format!("cannot run {}: {e}", argv[0])];
            return run;
        }
    };
    let started = Instant::now();
    let stdout = child.stdout.take();
    std::thread::scope(|scope| {
        if let Some(out) = stdout {
            scope.spawn(move || {
                for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                    if let Some(us) = line
                        .strip_prefix("out_time_us=")
                        .and_then(|v| v.trim().parse::<f64>().ok())
                    {
                        on_secs(us / 1e6);
                    }
                }
            });
        }
        watch(&mut child, &mut run, started, (err_file, stderr_cap), stop);
    });
    read_stderr(err_file, &mut run);
    let _ = std::fs::remove_file(err_file);
    run
}

// Poll the child until it exits or must be killed, recording why.
fn watch(
    child: &mut std::process::Child,
    run: &mut Run,
    started: Instant,
    (err_file, stderr_cap): (&Path, u64),
    stop: &dyn Fn() -> bool,
) {
    // Polled quickly at first, so the short sampling runs do not each wait out a full nap.
    let mut nap = Duration::from_millis(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
        let rss = rss_mib(child.id());
        run.peak_rss_mib = run.peak_rss_mib.max(rss);
        if rss > RSS_CAP_MIB {
            run.runaway = true;
        } else if std::fs::metadata(err_file).is_ok_and(|m| m.len() > stderr_cap) {
            run.overflow = true;
        } else if started.elapsed() > TIMEOUT {
            run.timed_out = true;
        } else if stop() {
            run.cancelled = true;
        } else {
            std::thread::sleep(nap);
            nap = (nap * 2).min(Duration::from_millis(500));
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
    if run.runaway || run.overflow || run.timed_out || run.cancelled {
        run.signal = None;
    }
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
/// `None` when stopped early. `progress` hears the stage (`reading`, `decoding`) and how
/// many seconds of the movie it has reached. A failure while the file or library is
/// unreachable is the share, not the file. A damaged file of known `duration` is then
/// sampled for [`forensic`]; a stop there keeps the verdict without it.
pub fn full_decode(
    ffmpeg: &Path,
    mkv: &Path,
    library: &Path,
    duration: Option<f64>,
    err_file: &Path,
    stop: &dyn Fn() -> bool,
    progress: &(dyn Fn(&'static str, f64) + Sync),
) -> Option<Verdict> {
    let reached = std::sync::Mutex::new(0f64);
    let heard = |stage: &'static str, t: f64| {
        let mut r = reached.lock().unwrap_or_else(|e| e.into_inner());
        *r = r.max(t);
        progress(stage, t);
    };
    let mut v = decode_stages(ffmpeg, mkv, library, err_file, stop, &heard)?;
    let secs = *reached.lock().unwrap_or_else(|e| e.into_inner());
    v.reached_secs = (secs > 0.0).then_some(secs);
    if v.completed
        && !v.clean
        && let Some(d) = duration.filter(|d| *d > 0.0)
    {
        v.forensic = forensic(ffmpeg, mkv, d, err_file, stop);
    }
    Some(v)
}

fn decode_stages(
    ffmpeg: &Path,
    mkv: &Path,
    library: &Path,
    err_file: &Path,
    stop: &dyn Fn() -> bool,
    progress: &(dyn Fn(&'static str, f64) + Sync),
) -> Option<Verdict> {
    let bin = ffmpeg.to_string_lossy();
    let file = mkv.to_string_lossy();
    let head = [
        &*bin,
        "-nostdin",
        "-nostats",
        "-progress",
        "pipe:1",
        "-v",
        "error",
        "-i",
        &*file,
    ];
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
    let run = run_monitored(&demux, err_file, stop, &|t| progress("reading", t));
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
    let mut run = run_monitored(&decode, err_file, stop, &|t| progress("decoding", t));
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

#[cfg(test)]
#[path = "deep_tests.rs"]
mod tests;
