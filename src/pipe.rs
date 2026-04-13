//! Pipe — the core operation. Open source stream, open dest stream, copy.
//!
//! freemkv <source_url> <dest_url> [flags]

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::IOStream;
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
    INTERRUPTED.store(true, Ordering::Relaxed);
}

pub fn run(source: &str, dest: &str, args: &[String]) {
    install_signal_handler();

    // Parse flags
    let mut verbose = false;
    let mut quiet = false;
    let mut keydb_path: Option<String> = None;
    let mut title_nums: Vec<usize> = Vec::new();
    let mut list_only = false;
    let mut all = false;
    let mut min_minutes: Option<u64> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" | "--verbose" => verbose = true,
            "-q" | "--quiet" => quiet = true,
            "-l" | "--list" => list_only = true,
            "-a" | "--all" => all = true,
            "--min" => {
                i += 1;
                min_minutes = args.get(i).and_then(|s| s.parse::<u64>().ok());
            }
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
            _ => {} // URLs handled by caller
        }
        i += 1;
    }

    // Validate: --all and -t together is an error
    if all && !title_nums.is_empty() {
        eprintln!("Error: --all and -t cannot be used together");
        std::process::exit(1);
    }

    // Warn if --min is used without --all
    if min_minutes.is_some() && !all {
        eprintln!("Warning: --min has no effect without --all");
    }

    let parsed_source = libfreemkv::parse_url(source);
    let is_disc = parsed_source.is_disc_source();

    // --all requires disc:// or iso:// source
    if all && !is_disc {
        eprintln!("--all requires disc:// or iso:// source");
        std::process::exit(1);
    }

    // Batch mode: --all or multiple -t values
    let batch = all || title_nums.len() > 1;

    if batch {
        run_batch(
            source,
            dest,
            &keydb_path,
            &title_nums,
            all,
            min_minutes,
            verbose,
            quiet,
            list_only,
        );
        return;
    }

    // Disc-to-ISO: raw sector dump (full disc clone)
    let parsed_dest = libfreemkv::parse_url(dest);
    if matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. })
        && matches!(parsed_dest, libfreemkv::StreamUrl::Iso { .. })
    {
        run_disc_to_iso(source, dest, &keydb_path, verbose, quiet);
        return;
    }

    // Single title mode (original behavior)
    let title_num = title_nums.first().map(|n| n - 1); // convert 1-based to 0-based

    let out = Output::new(verbose, quiet);

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);

    // Open input stream
    out.raw_inline(Normal, &format!("Opening {}... ", source));
    let input_opts = libfreemkv::InputOptions {
        keydb_path,
        title_index: title_num,
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

    // Show metadata
    print_stream_info(&out, &meta);

    if list_only {
        return;
    }

    // Open output stream
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

    // Pipe: source → dest
    let total_size = input.total_bytes();
    out.blank(Normal);
    out.raw_inline(Normal, "Copying... ");

    let start = std::time::Instant::now();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 192 * 1024]; // 1024 BD-TS packets
    let mut last_update = start;

    loop {
        if INTERRUPTED.load(Ordering::Relaxed) {
            break;
        }
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if output.write_all(&buf[..n]).is_err() {
                    break;
                }
                total += n as u64;

                let now = std::time::Instant::now();
                if now.duration_since(last_update).as_secs_f64() >= 2.0 {
                    let elapsed = start.elapsed().as_secs_f64();
                    let mb = total as f64 / (1024.0 * 1024.0);
                    let avg = mb / elapsed;

                    if let Some(total_sz) = total_size {
                        let pct = (total as f64 / total_sz as f64 * 100.0).min(100.0);
                        let eta = if avg > 0.0 {
                            let s = (total_sz - total) as f64 / (1024.0 * 1024.0) / avg;
                            format!("{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32)
                        } else {
                            "--:--".into()
                        };
                        let total_mb = total_sz as f64 / (1024.0 * 1024.0);
                        let (d, t) = if total_mb >= 1024.0 {
                            (
                                format!("{:.1} GB", mb / 1024.0),
                                format!("{:.1} GB", total_mb / 1024.0),
                            )
                        } else {
                            (format!("{:.0} MB", mb), format!("{:.0} MB", total_mb))
                        };
                        eprint!(
                            "\r  {} / {}  ({:.0}%)  {:.1} MB/s  ETA {}   ",
                            d, t, pct, avg, eta
                        );
                    } else {
                        let (d, u) = if mb >= 1024.0 {
                            (format!("{:.1}", mb / 1024.0), "GB")
                        } else {
                            (format!("{:.0}", mb), "MB")
                        };
                        eprint!("\r  {} {}  {:.1} MB/s   ", d, u, avg);
                    }
                    let _ = std::io::stderr().flush();
                    last_update = now;
                }
            }
            Err(_) => break,
        }
    }

    let _ = output.finish();

    let elapsed = start.elapsed().as_secs_f64();
    let mb = total as f64 / (1024.0 * 1024.0);
    let (sz, unit) = if mb >= 1024.0 {
        (mb / 1024.0, "GB")
    } else {
        (mb, "MB")
    };

    out.raw(Normal, "done");
    out.blank(Normal);
    out.raw(
        Normal,
        &format!(
            "  {:.1} {} in {:.0}s ({:.0} MB/s)",
            sz,
            unit,
            elapsed,
            mb / elapsed
        ),
    );
    out.raw(Normal, &format!("  {} → {}", source, dest));
}

fn print_stream_info(out: &Output, meta: &libfreemkv::DiscTitle) {
    out.raw(Normal, &format!("  Streams: {}", meta.streams.len()));
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
                    &format!("    {:?} {}{}", v.codec, v.resolution, label),
                );
            }
            libfreemkv::Stream::Audio(a) => {
                let label = if a.label.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", a.label)
                };
                out.raw(
                    Normal,
                    &format!("    {:?} {} {}{}", a.codec, a.channels, a.language, label),
                );
            }
            libfreemkv::Stream::Subtitle(s) => {
                out.raw(Normal, &format!("    {:?} {}", s.codec, s.language));
            }
        }
    }
    if meta.duration_secs > 0.0 {
        let d = meta.duration_secs;
        out.raw(
            Normal,
            &format!(
                "  Duration: {}:{:02}:{:02}",
                d as u64 / 3600,
                (d as u64 % 3600) / 60,
                d as u64 % 60
            ),
        );
    }
}

/// Batch rip: --all or multiple -t values. Uses disc:// source with lower-level APIs.
fn run_batch(
    source: &str,
    dest: &str,
    keydb_path: &Option<String>,
    title_nums: &[usize],
    all: bool,
    min_minutes: Option<u64>,
    verbose: bool,
    quiet: bool,
    list_only: bool,
) {
    let out = Output::new(verbose, quiet);

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);

    // Dest must be a directory-bearing URL (mkv:// or m2ts://) for batch output.
    // Parse dest to get scheme and directory.
    let parsed_dest = libfreemkv::parse_url(dest);
    let dest_path_str = parsed_dest.path_str();
    let dest_dir = std::path::Path::new(dest_path_str)
        .parent()
        .unwrap_or(std::path::Path::new("."));

    let out_dir = if dest_path_str.ends_with('/')
        || dest_path_str.ends_with(std::path::MAIN_SEPARATOR)
        || std::path::Path::new(dest_path_str).extension().is_none()
    {
        std::path::PathBuf::from(dest_path_str)
    } else {
        dest_dir.to_path_buf()
    };

    let ext = match parsed_dest.scheme() {
        "mkv" => "mkv",
        "m2ts" => "m2ts",
        _ => {
            eprintln!("Error: batch ripping requires mkv:// or m2ts:// destination");
            std::process::exit(1);
        }
    };

    let _ = std::fs::create_dir_all(&out_dir);

    // Open disc and scan
    let parsed_source = libfreemkv::parse_url(source);
    out.raw_inline(Normal, &format!("Opening {}... ", source));
    let source_path = parsed_source.path_str();
    let mut session = if !source_path.is_empty() {
        match libfreemkv::Drive::open(std::path::Path::new(source_path)) {
            Ok(s) => { out.raw(Normal, "OK"); s }
            Err(e) => {
                out.raw(Normal, "FAILED");
                eprintln!("  {}", e);
                std::process::exit(1);
            }
        }
    } else {
        match libfreemkv::find_drive() {
            Some(d) => { out.raw(Normal, "OK"); d }
            None => {
                out.raw(Normal, "FAILED");
                eprintln!("{}", strings::get("error.no_drive"));
                std::process::exit(1);
            }
        }
    };

    out.raw_inline(Normal, "Waiting for disc... ");
    match session.wait_ready() {
        Ok(_) => out.raw(Normal, "OK"),
        Err(e) => {
            out.raw(Normal, "FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    }

    out.raw_inline(Normal, "Scanning... ");
    let scan_opts = match keydb_path {
        Some(ref kp) => libfreemkv::ScanOptions::with_keydb(kp),
        None => libfreemkv::ScanOptions::default(),
    };
    let disc = match libfreemkv::Disc::scan(&mut session, &scan_opts) {
        Ok(d) => {
            out.raw(Normal, "OK");
            d
        }
        Err(e) => {
            out.raw(Normal, "FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    };

    // Determine disc name for filenames
    let disc_name = disc
        .meta_title
        .as_deref()
        .unwrap_or(&disc.volume_id)
        .replace(
            |c: char| !c.is_ascii_alphanumeric() && c != ' ' && c != '-' && c != '_',
            "",
        )
        .trim()
        .replace(' ', "_");
    let disc_name = if disc_name.is_empty() {
        "disc".to_string()
    } else {
        disc_name
    };

    // Determine which titles to rip
    let title_indices: Vec<usize> = if all {
        let min_secs = min_minutes.map(|m| m as f64 * 60.0).unwrap_or(0.0);
        disc.titles
            .iter()
            .enumerate()
            .filter(|(_, t)| t.duration_secs >= min_secs)
            .map(|(i, _)| i)
            .collect()
    } else {
        // Multiple -t values (1-based → 0-based)
        title_nums.iter().map(|n| n.saturating_sub(1)).collect()
    };

    // Show titles
    out.blank(Normal);
    out.raw(
        Normal,
        &format!(
            "Titles ({} total, {} selected):",
            disc.titles.len(),
            title_indices.len()
        ),
    );
    out.blank(Normal);
    for &idx in &title_indices {
        if idx < disc.titles.len() {
            let t = &disc.titles[idx];
            out.raw(
                Normal,
                &format!(
                    "  {:2}. {} — {:.1} GB — {}",
                    idx + 1,
                    t.duration_display(),
                    t.size_gb(),
                    t.playlist
                ),
            );
        }
    }

    if list_only {
        return;
    }
    out.blank(Normal);

    // Rip each title
    for &idx in &title_indices {
        if idx >= disc.titles.len() {
            eprintln!(
                "Warning: title {} out of range (disc has {}), skipping",
                idx + 1,
                disc.titles.len()
            );
            continue;
        }

        let title = &disc.titles[idx];
        let filename = format!("{}_t{}.{}", disc_name, idx + 1, ext);
        let out_path = out_dir.join(&filename);
        let dest_url = format!("{}://{}", ext, out_path.display());

        out.raw(
            Normal,
            &format!(
                "Ripping title {} ({}, {:.1} GB) -> {}",
                idx + 1,
                title.duration_display(),
                title.size_gb(),
                out_path.display()
            ),
        );

        // Open title reader
        let mut reader = match disc.open_title(&mut session, idx) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  Error opening title {}: {}", idx + 1, e);
                continue;
            }
        };

        // Open output
        let mut output = match libfreemkv::open_output(&dest_url, title) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  Error creating {}: {}", out_path.display(), e);
                continue;
            }
        };

        // Copy using read_batch (ContentReader API)
        let start = std::time::Instant::now();
        let mut total: u64 = 0;
        let total_size = title.size_bytes;
        let mut last_update = start;

        loop {
            match reader.read_batch() {
                Ok(Some(batch)) => {
                    if output.write_all(batch).is_err() {
                        break;
                    }
                    total += batch.len() as u64;

                    let now = std::time::Instant::now();
                    if now.duration_since(last_update).as_secs_f64() >= 2.0 {
                        let elapsed_s = start.elapsed().as_secs_f64();
                        let mb = total as f64 / (1024.0 * 1024.0);
                        let avg = mb / elapsed_s;

                        if total_size > 0 {
                            let pct = (total as f64 / total_size as f64 * 100.0).min(100.0);
                            let total_mb = total_size as f64 / (1024.0 * 1024.0);
                            let eta = if avg > 0.0 {
                                let s = (total_size - total) as f64 / (1024.0 * 1024.0) / avg;
                                format!("{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32)
                            } else {
                                "--:--".into()
                            };
                            let (d, t) = if total_mb >= 1024.0 {
                                (
                                    format!("{:.1} GB", mb / 1024.0),
                                    format!("{:.1} GB", total_mb / 1024.0),
                                )
                            } else {
                                (format!("{:.0} MB", mb), format!("{:.0} MB", total_mb))
                            };
                            eprint!(
                                "\r  {} / {}  ({:.0}%)  {:.1} MB/s  ETA {}   ",
                                d, t, pct, avg, eta
                            );
                        } else {
                            let (d, u) = if mb >= 1024.0 {
                                (format!("{:.1}", mb / 1024.0), "GB")
                            } else {
                                (format!("{:.0}", mb), "MB")
                            };
                            eprint!("\r  {} {}  {:.1} MB/s   ", d, u, avg);
                        }
                        let _ = std::io::stderr().flush();
                        last_update = now;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("  Read error: {}", e);
                    break;
                }
            }
        }

        let _ = output.finish();
        // Clear the progress line
        eprint!("\r                                                              \r");
        let _ = std::io::stderr().flush();

        let elapsed = start.elapsed().as_secs_f64();
        let mb = total as f64 / (1024.0 * 1024.0);
        let (sz, unit) = if mb >= 1024.0 {
            (mb / 1024.0, "GB")
        } else {
            (mb, "MB")
        };

        out.raw(
            Normal,
            &format!(
                "  done: {:.1} {} in {:.0}s ({:.0} MB/s)",
                sz,
                unit,
                elapsed,
                if elapsed > 0.0 { mb / elapsed } else { 0.0 }
            ),
        );
        out.blank(Normal);
    }

    out.raw(
        Normal,
        &format!("Batch complete: {} titles ripped", title_indices.len()),
    );
}

/// Raw disc-to-ISO: read every sector from the drive and write to file.
/// No demuxing, no title selection — full disc clone with decryption.
fn run_disc_to_iso(
    source: &str,
    dest: &str,
    keydb_path: &Option<String>,
    verbose: bool,
    quiet: bool,
) {
    let out = Output::new(verbose, quiet);
    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);

    // Open drive
    let parsed_source = libfreemkv::parse_url(source);
    out.raw_inline(Normal, "Opening drive... ");
    let mut drive = match &parsed_source {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => {
            libfreemkv::Drive::open(p).unwrap_or_else(|e| {
                out.raw(Normal, "FAILED");
                eprintln!("  {}", e);
                std::process::exit(1);
            })
        }
        _ => libfreemkv::find_drive().unwrap_or_else(|| {
            out.raw(Normal, "FAILED");
            eprintln!("{}", strings::get("error.no_drive"));
            std::process::exit(1);
        }),
    };
    out.raw(Normal, "OK");

    let _ = drive.wait_ready();
    let _ = drive.init();
    let _ = drive.probe_disc();

    // AACS handshake if needed
    let scan_opts = match keydb_path {
        Some(ref kp) => libfreemkv::ScanOptions::with_keydb(kp),
        None => libfreemkv::ScanOptions::default(),
    };
    // Scan disc to trigger AACS key resolution (needed for decrypted reads)
    out.raw_inline(Normal, "Scanning disc... ");
    let disc = match libfreemkv::Disc::scan(&mut drive, &scan_opts) {
        Ok(d) => {
            out.raw(Normal, "OK");
            d
        }
        Err(e) => {
            out.raw(Normal, "FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    };

    // Read capacity
    let total_sectors = match drive.read_capacity() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Cannot read disc capacity: {}", e);
            std::process::exit(1);
        }
    };
    let total_bytes = total_sectors as u64 * 2048;
    let disc_name = disc.volume_id.trim().to_string();
    let disc_name = if disc_name.is_empty() { "disc".to_string() } else { disc_name };

    out.raw(Normal, &format!(
        "Disc: {} ({:.1} GB, {} sectors)",
        disc_name,
        total_bytes as f64 / 1_073_741_824.0,
        total_sectors
    ));

    // Open output file
    let iso_path = match libfreemkv::parse_url(dest) {
        libfreemkv::StreamUrl::Iso { ref path } => path.clone(),
        _ => {
            eprintln!("Destination must be iso://path");
            std::process::exit(1);
        }
    };
    let file = match std::fs::File::create(&iso_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Cannot create {}: {}", iso_path.display(), e);
            std::process::exit(1);
        }
    };
    let mut writer = std::io::BufWriter::with_capacity(4 * 1024 * 1024, file);

    out.raw(Normal, &format!("Output: {}", iso_path.display()));
    out.blank(Normal);

    // Lock tray during dump
    drive.lock_tray();

    // Read all sectors in batches
    let batch_sectors: u16 = 64;
    let mut buf = vec![0u8; batch_sectors as usize * 2048];
    let mut lba: u32 = 0;
    let mut bytes_written: u64 = 0;
    let start = std::time::Instant::now();
    let mut last_report = std::time::Instant::now();

    while lba < total_sectors {
        if INTERRUPTED.load(Ordering::Relaxed) {
            out.blank(Normal);
            out.raw(Normal, "Interrupted.");
            break;
        }

        let remaining = total_sectors - lba;
        let count = (remaining as u16).min(batch_sectors);
        let byte_count = count as usize * 2048;
        buf.resize(byte_count, 0);

        match drive.read_disc(lba, count, &mut buf[..byte_count]) {
            Ok(_) => {
                if let Err(e) = writer.write_all(&buf[..byte_count]) {
                    eprintln!("\nWrite error: {}", e);
                    break;
                }
                lba += count as u32;
                bytes_written += byte_count as u64;
            }
            Err(_) => {
                // Zero-fill unreadable sectors and continue
                buf[..byte_count].fill(0);
                if let Err(e) = writer.write_all(&buf[..byte_count]) {
                    eprintln!("\nWrite error: {}", e);
                    break;
                }
                lba += count as u32;
                bytes_written += byte_count as u64;
            }
        }

        // Progress every 500ms
        if last_report.elapsed().as_millis() >= 500 {
            let elapsed = start.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 { bytes_written as f64 / elapsed / 1_048_576.0 } else { 0.0 };
            let pct = bytes_written as f64 / total_bytes as f64 * 100.0;
            let eta = if speed > 0.0 {
                let remaining_bytes = total_bytes - bytes_written;
                let secs = remaining_bytes as f64 / (speed * 1_048_576.0);
                format!("{}:{:02}", secs as u64 / 60, secs as u64 % 60)
            } else {
                "?:??".to_string()
            };
            eprint!(
                "\r  {:.1}% | {:.1} GB / {:.1} GB | {:.1} MB/s | ETA {}    ",
                pct,
                bytes_written as f64 / 1_073_741_824.0,
                total_bytes as f64 / 1_073_741_824.0,
                speed,
                eta
            );
            last_report = std::time::Instant::now();
        }
    }

    let _ = writer.flush();
    eprintln!();

    // Unlock tray
    drive.unlock_tray();

    let elapsed = start.elapsed().as_secs_f64();
    let speed = if elapsed > 0.0 { bytes_written as f64 / elapsed / 1_048_576.0 } else { 0.0 };
    out.raw(Normal, &format!(
        "Complete: {:.1} GB in {:.0}s ({:.1} MB/s)",
        bytes_written as f64 / 1_073_741_824.0,
        elapsed,
        speed,
    ));
    out.raw(Normal, &format!("Output: {}", iso_path.display()));
}
