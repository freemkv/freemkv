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

#[test]
fn a_run_that_never_started_is_inconclusive_not_corrupt() {
    let v = classify(&Run::default(), "decode", 1);
    assert_eq!(
        (v.completed, v.clean, v.reason.as_str()),
        (false, false, "not_run")
    );
    assert_eq!(v.errors, 0);
    let t = tempfile::tempdir().unwrap();
    let r = run_monitored(
        &["/nonexistent/ffmpeg"],
        &t.path().join("err"),
        &|| false,
        &|_| {},
    );
    assert!(!classify(&r, "decode", 1).completed);
    let r = run_monitored(
        &["true"],
        &t.path().join("no-such-dir/err"),
        &|| false,
        &|_| {},
    );
    assert!(!classify(&r, "decode", 1).completed);
}

#[cfg(unix)]
#[test]
fn a_stderr_flood_is_cut_off_and_judged() {
    let t = tempfile::tempdir().unwrap();
    let r = run_capped(
        &[
            "sh",
            "-c",
            "i=0; while [ $i -lt 20000 ]; do echo 'error while decoding' >&2; i=$((i+1)); done; exec sleep 30",
        ],
        &t.path().join("err"),
        1 << 16,
        &|| false,
        &|_| {},
    );
    assert!(r.overflow && !r.cancelled);
    let v = classify(&r, "decode", 1);
    assert!(v.completed && !v.clean);
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
        &|_| {},
    );
    assert_eq!((r.rc, r.flood, r.lines.len()), (Some(3), 1, 2));
    assert!(!err.exists());
    let stopped = run_monitored(&["sh", "-c", "sleep 30"], &err, &|| true, &|_| {});
    assert!(stopped.cancelled);
    let seen = std::sync::Mutex::new(Vec::new());
    run_monitored(
        &["sh", "-c", "echo out_time_us=2500000; echo progress=end"],
        &err,
        &|| false,
        &|t| seen.lock().unwrap().push(t),
    );
    assert_eq!(*seen.lock().unwrap(), [2.5]);
}

#[test]
fn a_nonzero_exit_without_error_lines_is_not_clean() {
    let v = classify(&run(1, &[]), "decode", 1);
    assert_eq!(
        (v.completed, v.clean, v.reason.as_str(), v.errors),
        (true, false, "nonzero_exit", 1)
    );
    let r = Run {
        signal: Some(libc::SIGTERM),
        ..Default::default()
    };
    let v = classify(&r, "decode", 1);
    assert_eq!(
        (v.completed, v.reason.as_str(), v.signal.as_deref()),
        (false, "killed", Some("SIGTERM"))
    );
}

// A fake ffmpeg: logs its arguments, prints `stderr_line`, exits `rc`.
#[cfg(unix)]
fn fake_ffmpeg(dir: &Path, stderr_line: &str, rc: i32) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;
    let (bin, log) = (dir.join("ffmpeg"), dir.join("calls.log"));
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\n{}exit {rc}\n",
        log.display(),
        if stderr_line.is_empty() {
            String::new()
        } else {
            format!("echo '{stderr_line}' >&2\n")
        }
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

#[cfg(unix)]
#[test]
fn full_decode_demuxes_then_decodes_and_stops_at_the_first_bad_stage() {
    let t = tempfile::tempdir().unwrap();
    let mkv = t.path().join("A.mkv");
    std::fs::write(&mkv, b"x").unwrap();
    let err = t.path().join("err");
    let (bin, log) = fake_ffmpeg(t.path(), "", 0);
    let v = full_decode(&bin, &mkv, t.path(), None, &err, &|| false, &|_, _| {}).unwrap();
    assert!(v.clean && v.completed, "{v:?}");
    let calls = std::fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].contains("-map 0 -c copy"), "{calls:?}");
    assert!(calls[1].contains("-map 0:v:0 -map 0:a?"), "{calls:?}");

    std::fs::remove_file(&log).unwrap();
    let (bin, log) = fake_ffmpeg(t.path(), "error while decoding", 1);
    let v = full_decode(&bin, &mkv, t.path(), None, &err, &|| false, &|_, _| {}).unwrap();
    assert_eq!(
        (v.stage.as_str(), v.reason.as_str()),
        ("demux", "demux_errors")
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);

    assert!(full_decode(&bin, &mkv, t.path(), None, &err, &|| true, &|_, _| {}).is_none());
}

#[cfg(unix)]
#[test]
fn a_damaged_file_is_sampled_for_where_and_how_it_fails() {
    use std::os::unix::fs::PermissionsExt as _;
    let t = tempfile::tempdir().unwrap();
    let mkv = t.path().join("A.mkv");
    std::fs::write(&mkv, b"x").unwrap();
    let (bin, log) = (t.path().join("ffmpeg"), t.path().join("calls.log"));
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$*\" in\n\
             *-progress*'-map 0:v:0'*) echo 'error while decoding MB 1 1' >&2; echo out_time_us=3000000000;;\n\
             *'-ss 750 '*'-map 0:v:0'*) echo 'error while decoding MB 3 3' >&2;;\n\
             *'-ss 1050 '*'-c copy'*) echo 'non monotonically increasing dts to muxer' >&2;;\n\
             esac\nexit 0\n",
        log.display()
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = t.path().join("err");
    let v = full_decode(
        &bin,
        &mkv,
        t.path(),
        Some(3600.0),
        &err,
        &|| false,
        &|_, _| {},
    )
    .unwrap();
    assert_eq!(
        (v.reason.as_str(), v.reached_secs),
        ("decode_errors", Some(3000.0))
    );
    let f = v.forensic.unwrap();
    assert_eq!(f.windows.len(), 12);
    assert_eq!(f.windows[2].at_secs, 750);
    assert_eq!(f.windows[2].payload_errors, 1);
    assert_eq!(f.windows[3].timestamp_errors, 1);
    assert_eq!(f.buckets, ["payload_bitstream", "timestamp"]);
    assert_eq!(f.pct_windows_corrupt, 8);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().lines().count(),
        2 + 24
    );
    // A stop while sampling keeps the verdict, without the sampling.
    let n = std::sync::atomic::AtomicUsize::new(0);
    let stop = || n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 3;
    let v = full_decode(&bin, &mkv, t.path(), Some(3600.0), &err, &stop, &|_, _| {});
    assert!(v.is_none_or(|v| v.forensic.is_none()));
}

#[cfg(unix)]
#[test]
fn a_failure_while_the_share_is_gone_is_inconclusive_not_corrupt() {
    let t = tempfile::tempdir().unwrap();
    let err = t.path().join("err");
    let (bin, _) = fake_ffmpeg(t.path(), "", 1);
    let mkv = t.path().join("A.mkv");
    std::fs::write(&mkv, b"x").unwrap();
    let v = full_decode(&bin, &mkv, t.path(), None, &err, &|| false, &|_, _| {}).unwrap();
    assert_eq!((v.completed, v.reason.as_str()), (true, "nonzero_exit"));
    std::fs::remove_file(&mkv).unwrap();
    let v = full_decode(&bin, &mkv, t.path(), None, &err, &|| false, &|_, _| {}).unwrap();
    assert_eq!(
        (v.completed, v.reason.as_str()),
        (false, "media_unavailable")
    );
    assert_eq!(v.errors, 0);
    std::fs::write(&mkv, b"x").unwrap();
    let gone = t.path().join("no-library");
    let v = full_decode(&bin, &mkv, &gone, None, &err, &|| false, &|_, _| {}).unwrap();
    assert!(!v.completed);
}
