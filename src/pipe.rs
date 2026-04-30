//! Pipe — stream in, stream out.
//!
//! One pipeline for everything:
//!   1. disc→ISO: Disc::copy() (not a stream)
//!   2. Everything else: input → PES → output, one title at a time
//!
//! Batch (multiple titles) is just a for loop calling pipe() per title.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::pes::Stream as PesStream;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn install_signal_handler() {
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        );
    }

    #[cfg(windows)]
    unsafe {
        extern "system" fn handler(_: u32) -> i32 {
            INTERRUPTED.store(true, Ordering::SeqCst);
            1
        }
        unsafe extern "system" {
            fn SetConsoleCtrlHandler(
                handler: unsafe extern "system" fn(u32) -> i32,
                add: i32,
            ) -> i32;
        }
        SetConsoleCtrlHandler(handler, 1);
    }
}

#[cfg(unix)]
extern "C" fn handle_sigint(_sig: libc::c_int) {
    if INTERRUPTED.load(Ordering::SeqCst) {
        unsafe { libc::_exit(130) };
    }
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Format an error for display using i18n strings.
fn fmt_err(e: &dyn std::fmt::Display) -> String {
    strings::fmt("error.generic", &[("detail", &e.to_string())])
}

// ── CLI entry point ─────────────────────────────────────────────────────────

/// Returns true on success, false on error.
pub fn run(source: &str, dest: &str, args: &[String]) -> bool {
    install_signal_handler();

    // Parse flags
    let mut verbose = false;
    let mut quiet = false;
    let mut raw = false;
    let mut multipass = false;
    let mut keydb_path: Option<String> = None;
    let mut title_nums: Vec<usize> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" | "--verbose" => verbose = true,
            "-q" | "--quiet" => quiet = true,
            "--raw" => raw = true,
            "--multipass" => multipass = true,
            "-t" | "--title" => {
                i += 1;
                if let Some(n) = args.get(i).and_then(|s| s.parse::<usize>().ok()) {
                    title_nums.push(n);
                }
            }
            "-k" | "--keydb" => {
                i += 1;
                keydb_path = args.get(i).cloned();
            }
            _ => {}
        }
        i += 1;
    }

    let out = Output::new(verbose, quiet);
    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);

    let parsed_source = libfreemkv::parse_url(source);
    let parsed_dest = libfreemkv::parse_url(dest);

    // Disc → ISO: not a stream, use Disc::copy()
    if matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. })
        && matches!(parsed_dest, libfreemkv::StreamUrl::Iso { .. })
    {
        disc_to_iso(source, dest, &keydb_path, raw, multipass, &out);
        return true;
    }

    // ISO → ISO + --multipass: patch bad ranges from the mapfile
    if matches!(parsed_source, libfreemkv::StreamUrl::Iso { .. })
        && matches!(parsed_dest, libfreemkv::StreamUrl::Iso { .. })
        && multipass
    {
        iso_patch(dest, raw, &out);
        return true;
    }

    // Everything else: figure out titles, pipe each one
    // For disc with explicit -t, skip scan_titles (pipe_disc does its own scan)
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });
    let titles = if is_disc && !title_nums.is_empty() {
        None // single title mode — pipe_disc handles scan
    } else {
        scan_titles(source, &keydb_path)
    };
    let is_dir_dest = dest.ends_with('/') || std::path::Path::new(parsed_dest.path_str()).is_dir();

    // Build the list of (title_index, dest_url) pairs
    let jobs: Vec<(Option<usize>, String)> = match &titles {
        Some(t) if !t.is_empty() => {
            // Source has titles — select which ones
            let indices: Vec<usize> = if title_nums.is_empty() {
                (0..t.len()).collect()
            } else {
                title_nums.iter().map(|n| n.saturating_sub(1)).collect()
            };

            if indices.len() == 1 && !is_dir_dest {
                // Single title to a single file
                vec![(Some(indices[0]), dest.to_string())]
            } else {
                // Multiple titles → directory
                let ext = parsed_dest.scheme();
                let dest_dir = std::path::Path::new(parsed_dest.path_str());
                let _ = std::fs::create_dir_all(dest_dir);
                let disc_name = t
                    .first()
                    .and_then(|ti| {
                        if ti.playlist.is_empty() {
                            None
                        } else {
                            Some(sanitize_name(&ti.playlist))
                        }
                    })
                    .unwrap_or_else(|| "disc".to_string());

                indices
                    .iter()
                    .map(|&idx| {
                        let filename = format!("{}_t{}.{}", disc_name, idx + 1, ext);
                        let url = format!("{}://{}", ext, dest_dir.join(filename).display());
                        (Some(idx), url)
                    })
                    .collect()
            }
        }
        _ => {
            // No title list — single pass, no title index
            let idx = title_nums.first().map(|n| n - 1);
            vec![(idx, dest.to_string())]
        }
    };

    // Show summary for multi-title
    if let Some(ref t) = titles {
        if jobs.len() > 1 {
            out.raw(
                Normal,
                &strings::fmt(
                    "rip.titles_summary",
                    &[
                        ("total", &t.len().to_string()),
                        ("selected", &jobs.len().to_string()),
                    ],
                ),
            );
            out.blank(Normal);
        }
    }

    // Pipe each title
    let mut ok = true;
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });

    for (title_idx, dest_url) in &jobs {
        // Print title info if we have it
        if let (Some(idx), Some(t)) = (title_idx, &titles) {
            if *idx >= t.len() {
                eprintln!(
                    "{}",
                    strings::fmt(
                        "rip.warning_title_range",
                        &[
                            ("num", &(idx + 1).to_string()),
                            ("count", &t.len().to_string()),
                        ]
                    )
                );
                continue;
            }
            let title = &t[*idx];
            out.raw(
                Normal,
                &strings::fmt(
                    "rip.title_info",
                    &[
                        ("num", &(idx + 1).to_string()),
                        ("duration", &title.duration_display()),
                        ("size", &format!("{:.1}", title.size_gb())),
                    ],
                ),
            );
        }

        if is_disc {
            // Disc source: use open_drive() directly — one session, no double init.
            if let Err(e) = pipe_disc(
                source,
                dest_url,
                title_idx.unwrap_or(0),
                &keydb_path,
                raw,
                multipass,
                &out,
            ) {
                out.raw(Normal, &fmt_err(&e));
                ok = false;
            }
        } else {
            // Non-disc: use input() as before
            let opts = libfreemkv::InputOptions {
                keydb_path: keydb_path.clone(),
                title_index: *title_idx,
                raw,
            };
            if let Err(e) = pipe(source, dest_url, &opts, &out) {
                out.raw(Normal, &fmt_err(&e));
                ok = false;
            }
        }
        out.blank(Normal);
    }

    ok
}

// ── The pipeline engine ─────────────────────────────────────────────────────

/// Disc source: one open, one scan, one stream. No double init.
fn pipe_disc(
    source: &str,
    dest: &str,
    title_idx: usize,
    keydb_path: &Option<String>,
    raw: bool,
    _multipass: bool,
    out: &Output,
) -> Result<(), String> {
    let parsed = libfreemkv::parse_url(source);
    let device = match &parsed {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => p.clone(),
        _ => libfreemkv::find_drive()
            .map(|d| std::path::PathBuf::from(d.device_path()))
            .ok_or_else(|| "No drive found".to_string())?,
    };

    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    let mut drive = libfreemkv::Drive::open(&device).map_err(|e| format!("{}", e))?;
    let _ = drive.wait_ready();
    let _ = drive.init();
    let _ = drive.probe_disc();
    drive.lock_tray();

    let scan_opts = match keydb_path {
        Some(p) => libfreemkv::ScanOptions {
            keydb_path: Some(p.into()),
        },
        None => libfreemkv::ScanOptions::default(),
    };
    let disc = libfreemkv::Disc::scan(&mut drive, &scan_opts).map_err(|e| format!("{}", e))?;

    if title_idx >= disc.titles.len() {
        return Err(format!(
            "Title {} out of range ({})",
            title_idx + 1,
            disc.titles.len()
        ));
    }

    let title = disc.titles[title_idx].clone();
    let keys = disc.decrypt_keys();
    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());
    let format = disc.content_format;

    let mut input = libfreemkv::DiscStream::new(Box::new(drive), title, keys, batch, format);

    if raw {
        input.set_raw();
    }

    out.raw(Normal, &strings::get("rip.ok"));

    // From here, same as pipe(): headers → output → frame loop
    let mut buffered = Vec::new();
    while !input.headers_ready() {
        match input.read() {
            Ok(Some(frame)) => buffered.push(frame),
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    let info = input.info().clone();
    print_stream_info(out, &info);

    let mut title = info.clone();
    let disc_name = disc.meta_title.as_deref().unwrap_or(&disc.volume_id);
    title.playlist = disc_name.to_string();
    title.codec_privates = (0..info.streams.len())
        .map(|i| input.codec_private(i))
        .collect();

    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", dest)]));
    let raw_output = match libfreemkv::output(dest, &title) {
        Ok(s) => {
            out.raw(Normal, &strings::get("rip.ok"));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };
    let mut output = libfreemkv::pes::CountingStream::new(raw_output);

    out.blank(Normal);

    let total_bytes = info.size_bytes;
    let start = std::time::Instant::now();
    let mut last_update = start;

    for frame in &buffered {
        output.write(frame).map_err(|e| format!("{}", e))?;
    }

    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            out.blank(Normal);
            out.raw(Normal, &strings::get("rip.interrupted"));
            break;
        }

        match input.read() {
            Ok(Some(frame)) => {
                output.write(&frame).map_err(|e| format!("{}", e))?;

                let now = std::time::Instant::now();
                if !out.is_quiet() && now.duration_since(last_update).as_secs_f64() >= 0.5 {
                    print_progress(output.bytes_written(), total_bytes, 0, &start);
                    last_update = now;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    output.finish().map_err(|e| format!("{}", e))?;

    if !out.is_quiet() {
        eprint!("\r                                                                    \r");
    }
    let done = output.bytes_written();
    let elapsed = start.elapsed().as_secs_f64();
    let mb = done as f64 / (1024.0 * 1024.0);
    let (sz, unit) = if mb >= 1024.0 {
        (mb / 1024.0, "GB")
    } else {
        (mb, "MB")
    };
    let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
    out.raw(
        Normal,
        &strings::fmt(
            "rip.complete",
            &[
                ("size", &format!("{sz:.1}")),
                ("unit", unit),
                ("time", &format!("{elapsed:.0}")),
                ("speed", &format!("{speed:.0}")),
            ],
        ),
    );
    Ok(())
}

/// One title: open input, open output, stream PES frames.
/// Used for non-disc sources (ISO, MKV, M2TS, network, stdio).
fn pipe(
    source: &str,
    dest: &str,
    opts: &libfreemkv::InputOptions,
    out: &Output,
) -> Result<(), String> {
    // Open input
    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    let mut input = match libfreemkv::input(source, opts) {
        Ok(s) => {
            out.raw(Normal, &strings::get("rip.ok"));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };

    // Read frames until codec headers are ready (also parses metadata headers for stdio/network)
    let mut buffered = Vec::new();
    while !input.headers_ready() {
        match input.read() {
            Ok(Some(frame)) => buffered.push(frame),
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    // Get info after header scanning (stdio/network populate info during read)
    let info = input.info().clone();
    print_stream_info(out, &info);

    // Build output title with codec_privates from input
    let mut title = info.clone();
    title.codec_privates = (0..info.streams.len())
        .map(|i| input.codec_private(i))
        .collect();

    // Open output, wrapped with byte counter for progress
    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", dest)]));
    let raw_output = match libfreemkv::output(dest, &title) {
        Ok(s) => {
            out.raw(Normal, &strings::get("rip.ok"));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };
    let mut output = libfreemkv::pes::CountingStream::new(raw_output);

    out.blank(Normal);

    let total_bytes = info.size_bytes;
    let start = std::time::Instant::now();
    let mut last_update = start;

    // Write buffered frames
    for frame in &buffered {
        output.write(frame).map_err(|e| format!("{}", e))?;
    }

    // Stream remaining frames
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            out.blank(Normal);
            out.raw(Normal, &strings::get("rip.interrupted"));
            break;
        }

        match input.read() {
            Ok(Some(frame)) => {
                output.write(&frame).map_err(|e| format!("{}", e))?;

                let now = std::time::Instant::now();
                if !out.is_quiet() && now.duration_since(last_update).as_secs_f64() >= 0.5 {
                    print_progress(output.bytes_written(), total_bytes, 0, &start);
                    last_update = now;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    output.finish().map_err(|e| format!("{}", e))?;

    if !out.is_quiet() {
        eprint!("\r                                                                    \r");
    }
    let done = output.bytes_written();
    let elapsed = start.elapsed().as_secs_f64();
    let mb = done as f64 / (1024.0 * 1024.0);
    let (sz, unit) = if mb >= 1024.0 {
        (mb / 1024.0, "GB")
    } else {
        (mb, "MB")
    };
    let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
    out.raw(
        Normal,
        &strings::fmt(
            "rip.complete",
            &[
                ("size", &format!("{sz:.1}")),
                ("unit", unit),
                ("time", &format!("{elapsed:.0}")),
                ("speed", &format!("{speed:.0}")),
            ],
        ),
    );
    Ok(())
}

// ── Disc → ISO (raw sector copy, not a stream) ────────────────────────────

fn disc_to_iso(source: &str, dest: &str, keydb_path: &Option<String>, raw: bool, multipass: bool, out: &Output) {
    let parsed_source = libfreemkv::parse_url(source);
    let device = match &parsed_source {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => Some(p.clone()),
        _ => None,
    };

    let mut drive = match device {
        Some(ref d) => match libfreemkv::Drive::open(d) {
            Ok(d) => d,
            Err(e) => {
                out.raw(Normal, &fmt_err(&e));
                return;
            }
        },
        None => match libfreemkv::find_drive() {
            Some(d) => d,
            None => {
                out.raw(Normal, &strings::get("error.no_drive"));
                return;
            }
        },
    };
    out.raw(
        Normal,
        &strings::fmt("rip.drive", &[("device", drive.device_path())]),
    );
    let _ = drive.wait_ready();
    let _ = drive.init();
    let _ = drive.probe_disc();

    let scan_opts = match keydb_path {
        Some(p) => libfreemkv::ScanOptions {
            keydb_path: Some(p.into()),
        },
        None => libfreemkv::ScanOptions::default(),
    };
    let disc = match libfreemkv::Disc::scan(&mut drive, &scan_opts) {
        Ok(d) => d,
        Err(e) => {
            out.raw(
                Normal,
                &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
            );
            return;
        }
    };

    let disc_name = sanitize_name(disc.meta_title.as_deref().unwrap_or(&disc.volume_id));
    let iso_path = match libfreemkv::parse_url(dest) {
        libfreemkv::StreamUrl::Iso { ref path } => path.clone(),
        _ => unreachable!(),
    };

    let total_bytes = disc.capacity_sectors as u64 * 2048;
    out.raw(
        Normal,
        &strings::fmt(
            "rip.disc_label",
            &[
                ("name", &disc_name),
                (
                    "size",
                    &format!("{:.1}", total_bytes as f64 / 1_073_741_824.0),
                ),
            ],
        ),
    );
    out.raw(
        Normal,
        &strings::fmt("rip.output", &[("path", &iso_path.display().to_string())]),
    );
    out.blank(Normal);

    drive.lock_tray();
    let start = std::time::Instant::now();
    let last_update = std::cell::Cell::new(start);
    let last_work_done = std::cell::Cell::new(None::<u64>);
    let last_speed_time = std::cell::Cell::new(start);

    struct CliProgress<'a> {
        out: &'a Output,
        last_update: &'a std::cell::Cell<std::time::Instant>,
        last_work_done: &'a std::cell::Cell<Option<u64>>,
        last_speed_time: &'a std::cell::Cell<std::time::Instant>,
    }
    impl libfreemkv::progress::Progress for CliProgress<'_> {
        fn report(&self, p: &libfreemkv::progress::PassProgress) {
            if self.out.is_quiet() {
                return;
            }
            let now = std::time::Instant::now();
            if now.duration_since(self.last_update.get()).as_secs_f64() < 0.5 {
                return;
            }
            self.last_update.set(now);

            let inst_speed = match self.last_work_done.get() {
                Some(prev) => {
                    let prev_time = self.last_speed_time.get();
                    let dt = now.duration_since(prev_time).as_secs_f64();
                    if dt > 0.0 {
                        (p.work_done.saturating_sub(prev) as f64 / 1_048_576.0) / dt
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            };
            self.last_work_done.set(Some(p.work_done));
            self.last_speed_time.set(now);

            print_disc_progress(
                p.work_done,
                p.work_total,
                p.bytes_good_total,
                p.bytes_bad_total,
                p.bytes_total_disc,
                inst_speed,
            );
        }
    }
    let progress = CliProgress {
        out,
        last_update: &last_update,
        last_work_done: &last_work_done,
        last_speed_time: &last_speed_time,
    };

    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());
    let copy_opts = libfreemkv::disc::CopyOptions {
        decrypt: !raw,
        resume: true,
        batch_sectors: Some(batch),
        skip_on_error: multipass,
        halt: None,
        progress: Some(&progress),
    };
    match disc.copy(&mut drive, &iso_path, &copy_opts) {
        Ok(r) => {
            if !out.is_quiet() {
                eprint!("\r                                                                    \r");
            }
            let elapsed = start.elapsed().as_secs_f64();
            let mb = r.bytes_total as f64 / (1024.0 * 1024.0);
            let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
            out.raw(
                Normal,
                &strings::fmt(
                    "rip.complete",
                    &[
                        ("size", &format!("{:.1}", mb / 1024.0)),
                        ("unit", "GB"),
                        ("time", &format!("{elapsed:.0}")),
                        ("speed", &format!("{speed:.0}")),
                    ],
                ),
            );
            if multipass {
                let gb_good = r.bytes_good as f64 / 1_073_741_824.0;
                let mb_bad = r.bytes_unreadable as f64 / 1_048_576.0;
                let mb_pending = r.bytes_pending as f64 / 1_048_576.0;
                out.raw(
                    Normal,
                    &strings::fmt(
                        "rip.mapfile_summary",
                        &[
                            ("good", &format!("{gb_good:.2}")),
                            ("unreadable", &format!("{mb_bad:.1}")),
                            ("pending", &format!("{mb_pending:.1}")),
                        ],
                    ),
                );
            }
        }
        Err(e) => {
            out.raw(Normal, &fmt_err(&e));
        }
    }

    drive.unlock_tray();
}

// ── ISO patch (retry pass) ──────────────────────────────────────────────────

fn iso_patch(dest: &str, raw: bool, out: &Output) {
    let iso_path = match libfreemkv::parse_url(dest) {
        libfreemkv::StreamUrl::Iso { ref path } => path.clone(),
        _ => unreachable!(),
    };

    let mapfile_path = libfreemkv::disc::mapfile_path_for(&iso_path);
    let map = match libfreemkv::disc::mapfile::Mapfile::load(&mapfile_path) {
        Ok(m) => m,
        Err(e) => {
            out.raw(Normal, &fmt_err(&e));
            return;
        }
    };
    let stats = map.stats();
    let bad_bytes = stats.bytes_pending + stats.bytes_unreadable;
    if bad_bytes == 0 {
        out.raw(Normal, &strings::get("rip.nothing_to_patch"));
        return;
    }

    out.raw(
        Normal,
        &strings::fmt(
            "rip.patch_start",
            &[
                ("good", &format!("{:.2}", stats.bytes_good as f64 / 1_073_741_824.0)),
                ("unreadable", &format!("{:.1}", stats.bytes_unreadable as f64 / 1_048_576.0)),
                ("pending", &format!("{:.1}", stats.bytes_pending as f64 / 1_048_576.0)),
            ],
        ),
    );

    let start = std::time::Instant::now();
    let last_update = std::cell::Cell::new(start);
    let last_work_done = std::cell::Cell::new(None::<u64>);
    let last_speed_time = std::cell::Cell::new(start);

    struct CliProgress<'a> {
        out: &'a Output,
        last_update: &'a std::cell::Cell<std::time::Instant>,
        last_work_done: &'a std::cell::Cell<Option<u64>>,
        last_speed_time: &'a std::cell::Cell<std::time::Instant>,
    }
    impl libfreemkv::progress::Progress for CliProgress<'_> {
        fn report(&self, p: &libfreemkv::progress::PassProgress) {
            if self.out.is_quiet() {
                return;
            }
            let now = std::time::Instant::now();
            if now.duration_since(self.last_update.get()).as_secs_f64() < 0.5 {
                return;
            }
            self.last_update.set(now);

            let inst_speed = match self.last_work_done.get() {
                Some(prev) => {
                    let prev_time = self.last_speed_time.get();
                    let dt = now.duration_since(prev_time).as_secs_f64();
                    if dt > 0.0 {
                        (p.work_done.saturating_sub(prev) as f64 / 1_048_576.0) / dt
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            };
            self.last_work_done.set(Some(p.work_done));
            self.last_speed_time.set(now);

            print_disc_progress(
                p.work_done,
                p.work_total,
                p.bytes_good_total,
                p.bytes_bad_total,
                p.bytes_total_disc,
                inst_speed,
            );
        }
    }
    let progress = CliProgress {
        out,
        last_update: &last_update,
        last_work_done: &last_work_done,
        last_speed_time: &last_speed_time,
    };

    let mut reader = match libfreemkv::find_drive() {
        Some(d) => d,
        None => {
            out.raw(Normal, &strings::get("error.no_drive"));
            return;
        }
    };

    let scan_opts = libfreemkv::ScanOptions::default();
    let disc = match libfreemkv::Disc::scan(&mut reader, &scan_opts) {
        Ok(d) => d,
        Err(e) => {
            out.raw(Normal, &fmt_err(&e));
            return;
        }
    };

    let patch_opts = libfreemkv::disc::PatchOptions {
        decrypt: !raw,
        block_sectors: Some(1),
        full_recovery: true,
        reverse: false,
        wedged_threshold: 50,
        halt: None,
        progress: Some(&progress),
    };

    match disc.patch(&mut reader, &iso_path, &patch_opts) {
        Ok(pr) => {
            if !out.is_quiet() {
                eprint!("\r                                                                    \r");
            }
            let elapsed = start.elapsed().as_secs_f64();
            let recovered = pr.bytes_recovered_this_pass;
            let speed = if elapsed > 0.0 {
                recovered as f64 / 1_048_576.0 / elapsed
            } else {
                0.0
            };
            out.raw(
                Normal,
                &strings::fmt(
                    "rip.patch_done",
                    &[
                        ("recovered", &format!("{:.1}", recovered as f64 / 1_048_576.0)),
                        ("unreadable", &format!("{:.1}", pr.bytes_unreadable as f64 / 1_048_576.0)),
                        ("pending", &format!("{:.1}", pr.bytes_pending as f64 / 1_048_576.0)),
                        ("time", &format!("{elapsed:.0}")),
                        ("speed", &format!("{speed:.0}")),
                    ],
                ),
            );
        }
        Err(e) => {
            out.raw(Normal, &fmt_err(&e));
        }
    }
}

// ── Title scanning ──────────────────────────────────────────────────────────

/// Scan any source for its title list. Returns None if source has no titles
/// (e.g. a single M2TS file, network stream).
fn scan_titles(source: &str, keydb_path: &Option<String>) -> Option<Vec<libfreemkv::DiscTitle>> {
    let parsed = libfreemkv::parse_url(source);
    let scan_opts = match keydb_path {
        Some(p) => libfreemkv::ScanOptions {
            keydb_path: Some(p.into()),
        },
        None => libfreemkv::ScanOptions::default(),
    };

    match parsed {
        libfreemkv::StreamUrl::Iso { ref path } => {
            let mut reader =
                libfreemkv::mux::iso::IsoSectorReader::open(&path.to_string_lossy()).ok()?;
            let capacity = reader.capacity();
            let disc = libfreemkv::Disc::scan_image(&mut reader, capacity, &scan_opts).ok()?;
            Some(disc.titles)
        }
        libfreemkv::StreamUrl::Disc { ref device } => {
            let mut drive = match device {
                Some(d) => libfreemkv::Drive::open(d).ok()?,
                None => libfreemkv::find_drive()?,
            };
            let _ = drive.wait_ready();
            let _ = drive.init();
            let _ = drive.probe_disc();
            let disc = libfreemkv::Disc::scan(&mut drive, &scan_opts).ok()?;
            Some(disc.titles)
        }
        _ => None,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn fmt_bytes(bytes: u64) -> String {
    let gb = bytes as f64 / 1_073_741_824.0;
    let mb = bytes as f64 / 1_048_576.0;
    let kb = bytes as f64 / 1024.0;
    if gb >= 1.0 {
        format!("{:.2} GB", gb)
    } else if mb >= 1.0 {
        format!("{:.1} MB", mb)
    } else if kb >= 1.0 {
        format!("{:.0} KB", kb)
    } else {
        format!("{} B", bytes)
    }
}

fn fmt_speed(mbps: f64) -> String {
    if mbps >= 1.0 {
        format!("{:.1} MB/s", mbps)
    } else if mbps * 1024.0 >= 1.0 {
        format!("{:.0} KB/s", mbps * 1024.0)
    } else if mbps > 0.0 {
        format!("{:.0} B/s", mbps * 1_048_576.0)
    } else {
        "stalled".into()
    }
}

fn fmt_eta(secs: f64) -> String {
    if secs <= 0.0 || secs.is_infinite() {
        return "?:??".into();
    }
    let h = secs as u64 / 3600;
    let m = (secs as u64 % 3600) / 60;
    let s = secs as u64 % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

fn print_disc_progress(
    work_done: u64,
    work_total: u64,
    bytes_good: u64,
    bytes_bad: u64,
    bytes_disc: u64,
    inst_speed_mbps: f64,
) {
    if work_total == 0 || bytes_disc == 0 {
        return;
    }
    let pct = (work_done as f64 / work_total as f64 * 100.0).min(100.0);
    let gb_done = work_done as f64 / 1_073_741_824.0;
    let gb_total = work_total as f64 / 1_073_741_824.0;
    let processed = bytes_good.saturating_add(bytes_bad);
    let readable_pct = if processed > 0 {
        bytes_good as f64 / processed as f64 * 100.0
    } else {
        100.0
    };
    let eta = if inst_speed_mbps > 0.01 {
        let s = (work_total - work_done) as f64 / 1_048_576.0 / inst_speed_mbps;
        fmt_eta(s)
    } else {
        "?:??".into()
    };
    eprint!(
        "\r  {:.1}/{:.1} GB ({:.1}%)  {}  ETA {}    {:.1}% readable    ",
        gb_done, gb_total, pct,
        fmt_speed(inst_speed_mbps),
        eta,
        readable_pct,
    );
    let _ = std::io::stderr().flush();
}

fn print_progress(done: u64, total: u64, resumed_from: u64, start: &std::time::Instant) {
    let elapsed = start.elapsed().as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }
    let mb_done = done as f64 / 1_048_576.0;
    let session_mb = (done - resumed_from) as f64 / 1_048_576.0;
    let avg = session_mb / elapsed;

    if total > 0 {
        let pct = (done as f64 / total as f64 * 100.0).min(100.0);
        let mb_total = total as f64 / 1_048_576.0;
        let eta = if avg > 0.0 {
            let s = (total - done) as f64 / 1_048_576.0 / avg;
            format!("{}:{:02}", s as u64 / 60, s as u64 % 60)
        } else {
            "?:??".into()
        };
        if mb_total >= 1024.0 {
            eprint!(
                "\r  {:.1} GB / {:.1} GB  ({:.1}%)  {:.1} MB/s  ETA {}    ",
                mb_done / 1024.0,
                mb_total / 1024.0,
                pct,
                avg,
                eta
            );
        } else {
            eprint!(
                "\r  {:.0} MB / {:.0} MB  ({:.1}%)  {:.1} MB/s  ETA {}    ",
                mb_done, mb_total, pct, avg, eta
            );
        }
    } else {
        eprint!("\r  {:.1} MB  {:.1} MB/s    ", mb_done, avg);
    }
    let _ = std::io::stderr().flush();
}

fn print_stream_info(out: &Output, meta: &libfreemkv::DiscTitle) {
    out.raw(
        Normal,
        &format!("  {}: {}", strings::get("disc.titles"), meta.streams.len()),
    );
    for s in &meta.streams {
        match s {
            libfreemkv::Stream::Video(v) => {
                let label = if v.label.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", v.label)
                };
                out.raw(
                    Normal,
                    &format!("    {} {}{}", v.codec, v.resolution, label),
                );
            }
            libfreemkv::Stream::Audio(a) => {
                let mut tags: Vec<String> = Vec::new();
                if let Some(key) = audio_purpose_key(a.purpose) {
                    tags.push(strings::get(key));
                }
                if a.secondary {
                    tags.push(strings::get("stream.secondary"));
                }
                if !a.label.is_empty() {
                    tags.push(a.label.clone());
                }
                let label = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", tags.join(", "))
                };
                out.raw(
                    Normal,
                    &format!("    {} {} {}{}", a.codec, a.channels, a.language, label),
                );
            }
            libfreemkv::Stream::Subtitle(s) => {
                out.raw(Normal, &format!("    {} {}", s.codec, s.language));
            }
        }
    }
    if meta.duration_secs > 0.0 {
        let d = meta.duration_secs;
        out.raw(
            Normal,
            &format!(
                "  {}: {}:{:02}:{:02}",
                strings::get("disc.format"),
                d as u64 / 3600,
                (d as u64 % 3600) / 60,
                d as u64 % 60
            ),
        );
    }
}

fn sanitize_name(name: &str) -> String {
    let s = name
        .replace(
            |c: char| !c.is_ascii_alphanumeric() && c != ' ' && c != '-' && c != '_',
            "",
        )
        .trim()
        .replace(' ', "_");
    if s.is_empty() { "disc".to_string() } else { s }
}

/// Map `LabelPurpose` to its locale string key. `Normal` → no tag.
fn audio_purpose_key(p: libfreemkv::LabelPurpose) -> Option<&'static str> {
    match p {
        libfreemkv::LabelPurpose::Commentary => Some("stream.purpose.commentary"),
        libfreemkv::LabelPurpose::Descriptive => Some("stream.purpose.descriptive"),
        libfreemkv::LabelPurpose::Score => Some("stream.purpose.score"),
        libfreemkv::LabelPurpose::Ime => Some("stream.purpose.ime"),
        libfreemkv::LabelPurpose::Normal => None,
    }
}
