//! Pipe — stream in, decrypt, stream out.
//!
//! freemkv <source_url> <dest_url> [flags]
//!
//! The pipeline:
//!   1. open_input(source, title)
//!   2. scan_keys(source) — for encrypted sources
//!   3. open_output(dest)
//!   4. loop: read → decrypt → write
//!
//! Streams are dumb pipes — they read/write bytes. They don't know
//! about encryption. Decryption is a pipeline step, not a stream concern.
//!
//! Batch (multiple titles) is a CLI concern — the CLI calls pipe()
//! once per title. The pipeline handles one stream at a time.
//!
//! Disc-to-ISO is a special case: raw sector copy with resume support.
//! Still uses the same read → write loop, just with a file instead of
//! a muxed output stream.

use crate::output::{Level::Normal, Output};
use crate::strings;
use std::io::{Read, Write};
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
            INTERRUPTED.store(true, Ordering::Relaxed);
            1
        }
        extern "system" {
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
    if INTERRUPTED.load(Ordering::Relaxed) {
        std::process::exit(130);
    }
    INTERRUPTED.store(true, Ordering::Relaxed);
}

// ── CLI entry point ─────────────────────────────────────────────────────────

pub fn run(source: &str, dest: &str, args: &[String]) {
    install_signal_handler();

    // Parse flags
    let mut verbose = false;
    let mut quiet = false;
    let mut raw = false;
    let mut keydb_path: Option<String> = None;
    let mut title_nums: Vec<usize> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" | "--verbose" => verbose = true,
            "-q" | "--quiet" => quiet = true,
            "--raw" => raw = true,
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
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });
    let is_iso_dest = matches!(parsed_dest, libfreemkv::StreamUrl::Iso { .. });

    // Disc → ISO: special case (raw sector copy with resume)
    if is_disc && is_iso_dest {
        disc_to_iso(source, dest, &keydb_path, raw, &out);
        return;
    }

    // Disc source: open drive, get title list, then pipe each title
    if is_disc {
        disc_to_stream(source, dest, &parsed_dest, &keydb_path, &title_nums, raw, &out);
        return;
    }

    // Non-disc source: determine titles and pipe each one
    let is_batch = dest.ends_with('/') || std::path::Path::new(parsed_dest.path_str()).is_dir();

    if is_batch {
        // Batch: need title list from source
        batch_stream(source, dest, &parsed_dest, &keydb_path, &title_nums, raw, &out);
    } else {
        // Single title
        let title_index = title_nums.first().map(|n| n - 1);
        pipe(source, dest, &keydb_path, title_index, raw, &out);
    }
}

// ── The pipeline engine ─────────────────────────────────────────────────────

/// The core pipeline. Opens source, opens dest, reads → decrypts → writes.
/// One title, one stream. Called once per title.
fn pipe(
    source: &str,
    dest: &str,
    keydb_path: &Option<String>,
    title_index: Option<usize>,
    raw: bool,
    out: &Output,
) {
    out.raw_inline(Normal, &format!("Opening {}... ", source));
    let input_opts = libfreemkv::InputOptions {
        keydb_path: keydb_path.clone(),
        title_index,
    };
    let mut input = match libfreemkv::open_input(source, &input_opts) {
        Ok(s) => {
            out.raw(Normal, "OK");
            s
        }
        Err(e) => {
            out.raw(Normal, "FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    };

    let meta = input.info().clone();
    print_stream_info(out, &meta);

    let keys = if raw {
        libfreemkv::DecryptKeys::None
    } else {
        scan_keys(source, keydb_path)
    };

    out.raw_inline(Normal, &format!("Opening {}... ", dest));
    let mut output = match libfreemkv::open_output(dest, &meta) {
        Ok(s) => {
            out.raw(Normal, "OK");
            s
        }
        Err(e) => {
            out.raw(Normal, "FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    };

    let total_bytes = input.total_bytes().unwrap_or(0);
    out.blank(Normal);
    copy_loop(&mut *input, &mut *output, &keys, total_bytes, 0, out);
    let _ = output.finish();
}

// ── Disc → ISO (special case: raw sector copy with resume) ──────────────────

fn disc_to_iso(
    source: &str,
    dest: &str,
    keydb_path: &Option<String>,
    raw: bool,
    out: &Output,
) {
    let parsed_source = libfreemkv::parse_url(source);
    let device = match &parsed_source {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => Some(p.clone()),
        _ => None,
    };

    let event_handler = |event: libfreemkv::Event| {
        use libfreemkv::EventKind::*;
        match event.kind {
            DriveOpened { ref device } => {
                out.raw(Normal, &strings::fmt("rip.opening", &[("device", device)]));
            }
            DriveReady => {
                out.raw(Normal, &strings::get("rip.ok"));
            }
            InitComplete { success } => {
                if !success {
                    out.raw(Normal, &strings::fmt("rip.continuing_oem", &[("error", "init")]));
                }
            }
            ScanComplete { titles } => {
                out.raw(Normal, &strings::fmt("rip.titles", &[("count", &titles.to_string())]));
            }
            _ => {}
        }
    };

    let result = match libfreemkv::DiscStream::open(
        device.as_deref(),
        keydb_path.as_deref(),
        0,
        Some(&event_handler),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let disc = &result.disc;
    let keys = if raw {
        libfreemkv::DecryptKeys::None
    } else {
        disc.decrypt_keys()
    };
    let mut drive = result.stream.into_drive();
    let disc_name = sanitize_name(
        disc.meta_title.as_deref().unwrap_or(&disc.volume_id),
    );

    let iso_path = match libfreemkv::parse_url(dest) {
        libfreemkv::StreamUrl::Iso { ref path } => path.clone(),
        _ => unreachable!(),
    };

    let total_bytes = disc.capacity_sectors as u64 * 2048;

    // Resume: check for existing partial ISO
    let (start_lba, resume_bytes) = match std::fs::metadata(&iso_path) {
        Ok(meta) if meta.len() > 0 => {
            let existing = meta.len();
            let safe_sectors = (existing / 2048).saturating_sub(5) as u32;
            let resume_from = safe_sectors as u64 * 2048;
            out.raw(
                Normal,
                &format!(
                    "Resuming: {:.1} GB already written, starting at sector {}",
                    resume_from as f64 / 1_073_741_824.0,
                    safe_sectors
                ),
            );
            (safe_sectors, resume_from as u64)
        }
        _ => (0u32, 0u64),
    };

    let title = libfreemkv::DiscTitle::empty();
    let mut input = if start_lba > 0 {
        libfreemkv::DiscStream::full_disc_resume(drive, title, disc.capacity_sectors, start_lba)
    } else {
        libfreemkv::DiscStream::full_disc(drive, title, disc.capacity_sectors)
    };
    input.lock_tray();

    out.raw(
        Normal,
        &format!(
            "Disc: {} ({:.1} GB, {} sectors)",
            disc_name,
            total_bytes as f64 / 1_073_741_824.0,
            disc.capacity_sectors
        ),
    );

    // Open file: append if resuming, create if new
    let file = if start_lba > 0 {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&iso_path)
            .unwrap_or_else(|e| {
                eprintln!("Cannot open {}: {}", iso_path.display(), e);
                std::process::exit(1);
            });
        f.set_len(resume_bytes).unwrap_or_else(|e| {
            eprintln!("Cannot truncate {}: {}", iso_path.display(), e);
            std::process::exit(1);
        });
        use std::io::Seek;
        let mut f = f;
        f.seek(std::io::SeekFrom::End(0)).unwrap();
        f
    } else {
        std::fs::File::create(&iso_path).unwrap_or_else(|e| {
            eprintln!("Cannot create {}: {}", iso_path.display(), e);
            std::process::exit(1);
        })
    };
    let mut output = std::io::BufWriter::with_capacity(4 * 1024 * 1024, file);

    out.raw(Normal, &format!("Output: {}", iso_path.display()));
    out.blank(Normal);
    copy_loop(&mut input, &mut output, &keys, total_bytes, resume_bytes, out);
    let _ = output.flush();
    input.unlock_tray();
}

// ── Disc → stream (title extraction with batch support) ─────────────────────

fn disc_to_stream(
    source: &str,
    dest: &str,
    parsed_dest: &libfreemkv::StreamUrl,
    keydb_path: &Option<String>,
    title_nums: &[usize],
    raw: bool,
    out: &Output,
) {
    let parsed_source = libfreemkv::parse_url(source);
    let device = match &parsed_source {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => Some(p.clone()),
        _ => None,
    };

    let event_handler = |event: libfreemkv::Event| {
        use libfreemkv::EventKind::*;
        match event.kind {
            DriveOpened { ref device } => {
                out.raw(Normal, &strings::fmt("rip.opening", &[("device", device)]));
            }
            DriveReady => {
                out.raw(Normal, &strings::get("rip.ok"));
            }
            InitComplete { success } => {
                if !success {
                    out.raw(Normal, &strings::fmt("rip.continuing_oem", &[("error", "init")]));
                }
            }
            ScanComplete { titles } => {
                out.raw(Normal, &strings::fmt("rip.titles", &[("count", &titles.to_string())]));
            }
            _ => {}
        }
    };

    let result = match libfreemkv::DiscStream::open(
        device.as_deref(),
        keydb_path.as_deref(),
        0,
        Some(&event_handler),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let disc = &result.disc;
    let keys = if raw {
        libfreemkv::DecryptKeys::None
    } else {
        disc.decrypt_keys()
    };
    let mut drive = result.stream.into_drive();
    let disc_name = sanitize_name(
        disc.meta_title.as_deref().unwrap_or(&disc.volume_id),
    );

    let title_indices: Vec<usize> = if title_nums.is_empty() {
        (0..disc.titles.len()).collect()
    } else {
        title_nums.iter().map(|n| n.saturating_sub(1)).collect()
    };

    let batch = title_indices.len() > 1;
    let ext = parsed_dest.scheme();

    if batch {
        out.raw(
            Normal,
            &format!("Titles ({} total, {} selected):", disc.titles.len(), title_indices.len()),
        );
        out.blank(Normal);
        for &idx in &title_indices {
            if idx < disc.titles.len() {
                let t = &disc.titles[idx];
                out.raw(
                    Normal,
                    &format!("  {:2}. {} — {:.1} GB — {}", idx + 1, t.duration_display(), t.size_gb(), t.playlist),
                );
            }
        }
        out.blank(Normal);
    }

    let dest_path = parsed_dest.path_str();
    let out_dir = if batch {
        let p = std::path::Path::new(dest_path);
        let dir = if p.extension().is_none() || dest_path.ends_with('/') {
            p.to_path_buf()
        } else {
            p.parent().unwrap_or(std::path::Path::new(".")).to_path_buf()
        };
        let _ = std::fs::create_dir_all(&dir);
        Some(dir)
    } else {
        None
    };

    drive.lock_tray();

    for &idx in &title_indices {
        if idx >= disc.titles.len() {
            eprintln!("Warning: title {} out of range (disc has {}), skipping", idx + 1, disc.titles.len());
            continue;
        }

        let title = disc.titles[idx].clone();
        let total_bytes = title.size_bytes;

        let dest_url = if let Some(ref dir) = out_dir {
            let filename = format!("{}_t{}.{}", disc_name, idx + 1, ext);
            format!("{}://{}", ext, dir.join(filename).display())
        } else {
            dest.to_string()
        };

        if batch {
            out.raw(Normal, &format!("Ripping title {} ({}, {:.1} GB)", idx + 1, title.duration_display(), title.size_gb()));
        } else {
            print_stream_info(out, &title);
        }

        let input = libfreemkv::DiscStream::title(drive, title.clone());

        out.raw_inline(Normal, &format!("Opening {}... ", dest_url));
        let mut output = match libfreemkv::open_output(&dest_url, &title) {
            Ok(s) => {
                out.raw(Normal, "OK");
                s
            }
            Err(e) => {
                out.raw(Normal, "FAILED");
                eprintln!("  {}", e);
                drive = input.into_drive();
                continue;
            }
        };

        out.blank(Normal);
        let mut input = input;
        copy_loop(&mut input, &mut *output, &keys, total_bytes, 0, out);
        let _ = output.finish();

        drive = input.into_drive();
        out.blank(Normal);
    }

    drive.unlock_tray();
}

// ── Batch from non-disc source (ISO, etc.) ──────────────────────────────────

fn batch_stream(
    source: &str,
    dest: &str,
    parsed_dest: &libfreemkv::StreamUrl,
    keydb_path: &Option<String>,
    title_nums: &[usize],
    raw: bool,
    out: &Output,
) {
    // Scan the source for title list
    let titles = match scan_titles(source, keydb_path) {
        Some(t) => t,
        None => {
            // Source doesn't have titles — treat as single
            pipe(source, dest, keydb_path, None, raw, out);
            return;
        }
    };

    let title_indices: Vec<usize> = if title_nums.is_empty() {
        (0..titles.len()).collect()
    } else {
        title_nums.iter().map(|n| n.saturating_sub(1)).collect()
    };

    let ext = parsed_dest.scheme();
    let dest_path = parsed_dest.path_str();
    let dir = std::path::Path::new(dest_path);
    let _ = std::fs::create_dir_all(dir);

    let disc_name = "disc"; // TODO: get from scan

    out.raw(
        Normal,
        &format!("Titles ({} total, {} selected):", titles.len(), title_indices.len()),
    );
    out.blank(Normal);

    for &idx in &title_indices {
        if idx >= titles.len() {
            eprintln!("Warning: title {} out of range (has {}), skipping", idx + 1, titles.len());
            continue;
        }

        let filename = format!("{}_t{}.{}", disc_name, idx + 1, ext);
        let dest_url = format!("{}://{}", ext, dir.join(filename).display());

        out.raw(Normal, &format!("Title {} → {}", idx + 1, dest_url));
        pipe(source, &dest_url, keydb_path, Some(idx), raw, out);
        out.blank(Normal);
    }
}

/// Scan a source for its title list. Returns None if source doesn't have titles.
fn scan_titles(source: &str, keydb_path: &Option<String>) -> Option<Vec<libfreemkv::DiscTitle>> {
    let parsed = libfreemkv::parse_url(source);
    match parsed {
        libfreemkv::StreamUrl::Iso { ref path } => {
            let scan_opts = match keydb_path {
                Some(p) => libfreemkv::ScanOptions::with_keydb(p),
                None => libfreemkv::ScanOptions::default(),
            };
            if let Ok(mut reader) = libfreemkv::mux::iso::IsoSectorReader::open(&path.to_string_lossy()) {
                let capacity = reader.capacity();
                if let Ok(disc) = libfreemkv::Disc::scan_image(&mut reader, capacity, &scan_opts) {
                    return Some(disc.titles);
                }
            }
            None
        }
        _ => None,
    }
}

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Scan a source for decryption keys. Returns DecryptKeys::None for
/// sources that don't have encryption metadata.
fn scan_keys(source: &str, keydb_path: &Option<String>) -> libfreemkv::DecryptKeys {
    let parsed = libfreemkv::parse_url(source);
    match parsed {
        libfreemkv::StreamUrl::Iso { ref path } => {
            let scan_opts = match keydb_path {
                Some(p) => libfreemkv::ScanOptions::with_keydb(p),
                None => libfreemkv::ScanOptions::default(),
            };
            if let Ok(mut reader) = libfreemkv::mux::iso::IsoSectorReader::open(&path.to_string_lossy()) {
                let capacity = reader.capacity();
                if let Ok(disc) = libfreemkv::Disc::scan_image(&mut reader, capacity, &scan_opts) {
                    return disc.decrypt_keys();
                }
            }
            libfreemkv::DecryptKeys::None
        }
        _ => libfreemkv::DecryptKeys::None,
    }
}

/// The copy loop: read → decrypt → write, with progress.
fn copy_loop(
    input: &mut dyn Read,
    output: &mut dyn Write,
    keys: &libfreemkv::DecryptKeys,
    total_bytes: u64,
    bytes_already: u64,
    out: &Output,
) {
    let start = std::time::Instant::now();
    let mut total: u64 = bytes_already;
    let mut buf = vec![0u8; 192 * 1024];
    let mut last_update = start;

    loop {
        if INTERRUPTED.load(Ordering::Relaxed) {
            out.blank(Normal);
            out.raw(Normal, "Interrupted.");
            break;
        }

        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                libfreemkv::decrypt_sectors(&mut buf[..n], keys, 0);

                if output.write_all(&buf[..n]).is_err() {
                    break;
                }
                total += n as u64;

                let now = std::time::Instant::now();
                if !out.is_quiet() && now.duration_since(last_update).as_secs_f64() >= 0.5 {
                    print_progress(total, total_bytes, bytes_already, &start);
                    last_update = now;
                }
            }
            Err(_) => break,
        }
    }

    if !out.is_quiet() {
        eprint!("\r                                                                    \r");
    }
    let elapsed = start.elapsed().as_secs_f64();
    let session_mb = (total - bytes_already) as f64 / (1024.0 * 1024.0);
    let total_mb = total as f64 / (1024.0 * 1024.0);
    let (sz, unit) = if total_mb >= 1024.0 {
        (total_mb / 1024.0, "GB")
    } else {
        (total_mb, "MB")
    };
    out.raw(
        Normal,
        &format!(
            "Complete: {:.1} {} in {:.0}s ({:.0} MB/s)",
            sz, unit, elapsed,
            if elapsed > 0.0 { session_mb / elapsed } else { 0.0 }
        ),
    );
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
                mb_done / 1024.0, mb_total / 1024.0, pct, avg, eta
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
    out.raw(Normal, &format!("  Streams: {}", meta.streams.len()));
    for s in &meta.streams {
        match s {
            libfreemkv::Stream::Video(v) => {
                let label = if v.label.is_empty() { String::new() } else { format!(" — {}", v.label) };
                out.raw(Normal, &format!("    {} {}{}", v.codec, v.resolution, label));
            }
            libfreemkv::Stream::Audio(a) => {
                let label = if a.label.is_empty() { String::new() } else { format!(" — {}", a.label) };
                out.raw(Normal, &format!("    {} {} {}{}", a.codec, a.channels, a.language, label));
            }
            libfreemkv::Stream::Subtitle(s) => {
                out.raw(Normal, &format!("    {} {}", s.codec, s.language));
            }
        }
    }
    if meta.duration_secs > 0.0 {
        let d = meta.duration_secs;
        out.raw(Normal, &format!("  Duration: {}:{:02}:{:02}", d as u64 / 3600, (d as u64 % 3600) / 60, d as u64 % 60));
    }
}

fn sanitize_name(name: &str) -> String {
    let s = name
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != ' ' && c != '-' && c != '_', "")
        .trim()
        .replace(' ', "_");
    if s.is_empty() { "disc".to_string() } else { s }
}
