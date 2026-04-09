//! freemkv rip — Back up a disc.
//!
//! Opens the drive, scans the disc (UDF, playlists, AACS — all automatic),
//! shows what's on it, and backs up selected titles.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Install SIGINT handler that sets the INTERRUPTED flag.
fn install_signal_handler() {
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as libc::sighandler_t);
    }
}

extern "C" fn handle_sigint(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

pub fn run(args: &[String]) {
    let mut device_path: Option<String> = None;
    let mut output_dir: Option<String> = None;
    let mut keydb_path: Option<String> = None;
    let mut title_num: Option<usize> = None;
    let mut list_only = false;
    let mut _raw = false;
    let mut duration_secs: Option<u64> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--device" | "-d" => {
                i += 1;
                device_path = args.get(i).cloned();
            }
            "--output" | "-o" => {
                i += 1;
                output_dir = args.get(i).cloned();
            }
            "--keydb" | "-k" => {
                i += 1;
                keydb_path = args.get(i).cloned();
            }
            "--title" | "-t" => {
                i += 1;
                title_num = args.get(i).and_then(|s| s.parse().ok());
            }
            "--list" | "-l" => {
                list_only = true;
            }
            "--raw" => {
                _raw = true;
            }
            "--duration" => {
                i += 1;
                duration_secs = args.get(i).and_then(|s| s.parse().ok());
            }
            _ => {
                if device_path.is_none() && args[i].starts_with("/dev/") {
                    device_path = Some(args[i].clone());
                }
            }
        }
        i += 1;
    }

    // Resolve device — auto-detect or validate user input
    let device = if let Some(user_path) = device_path {
        match libfreemkv::resolve_device(&user_path) {
            Ok((resolved, Some(warning))) => {
                eprintln!("  {}", warning);
                resolved
            }
            Ok((resolved, None)) => resolved,
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    } else {
        match libfreemkv::find_drive() {
            Some(d) => d,
            None => {
                eprintln!("No optical drive found. Specify with --device /dev/sgN");
                std::process::exit(1);
            }
        }
    };

    println!("freemkv rip v{}", env!("CARGO_PKG_VERSION"));
    println!();

    // Step 1: Open drive + wait for disc
    print!("Opening {}... ", device);
    let mut session = match libfreemkv::DriveSession::open(Path::new(&device)) {
        Ok(s) => s,
        Err(e) => {
            println!("FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    };
    println!("OK");
    println!("  {} {}", session.drive_id.vendor_id.trim(), session.drive_id.product_id.trim());

    print!("Waiting for disc... ");
    match session.wait_ready() {
        Ok(_) => println!("OK"),
        Err(e) => {
            println!("FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    }

    // Step 2: Init (unlock + firmware) + probe disc
    print!("Initializing drive... ");
    match session.init() {
        Ok(_) => {
            println!("OK");

            print!("Probing disc... ");
            match session.probe_disc() {
                Ok(_) => println!("OK"),
                Err(e) => {
                    println!("FAILED");
                    eprintln!("  {}", e);
                }
            }
        }
        Err(e) => {
            println!("FAILED");
            eprintln!("  {}", e);
            eprintln!("  Continuing without init (OEM speed)");
        }
    }

    // Step 3: Scan disc (UDF + playlists + AACS — all automatic)
    print!("Scanning disc... ");
    let scan_opts = if let Some(kp) = &keydb_path {
        libfreemkv::ScanOptions::with_keydb(kp)
    } else {
        libfreemkv::ScanOptions::default()
    };

    let disc = match libfreemkv::Disc::scan(&mut session, &scan_opts) {
        Ok(d) => {
            println!("OK");
            d
        }
        Err(e) => {
            println!("FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    };

    // Step 4: Show disc info
    println!();
    println!("  Capacity: {:.1} GB ({} sectors)", disc.capacity_gb(), disc.capacity_sectors);
    if disc.encrypted {
        if disc.aacs.is_some() {
            println!("  AACS:     encrypted (keys found)");
        } else {
            println!("  AACS:     encrypted (NO KEYS — cannot decrypt)");
        }
    } else {
        println!("  AACS:     not encrypted");
    }

    if let Some(ref aacs) = disc.aacs {
        println!("  VUK:      {:02x}{:02x}{:02x}{:02x}...",
            aacs.vuk[0], aacs.vuk[1], aacs.vuk[2], aacs.vuk[3]);
        println!("  Keys:     {} unit key(s)", aacs.unit_keys.len());
        if aacs.bus_encryption {
            println!("  Bus enc:  yes (AACS 2.0)");
        }
    }

    println!();
    println!("Titles ({}):", disc.titles.len());
    println!();

    // Title numbering is 1-based for users. Title 1 = longest (main feature).
    let target_idx = title_num.unwrap_or(1).saturating_sub(1);

    for (i, title) in disc.titles.iter().enumerate() {
        let num = i + 1;
        let marker = if i == target_idx { ">" } else { " " };
        println!("{} {:2}. {} — {:.1} GB — {} clip(s) — {}",
            marker, num, title.duration_display(), title.size_gb(),
            title.clips.len(), title.playlist);

        for stream in &title.streams {
            match stream {
                libfreemkv::Stream::Video(v) => println!("       Video: {:?} {}", v.codec, v.resolution),
                libfreemkv::Stream::Audio(a) => println!("       Audio: {:?} {} {}", a.codec, a.channels, a.language),
                libfreemkv::Stream::Subtitle(s) => println!("       Sub:   {}", s.language),
            }
        }
    }

    if list_only {
        return;
    }

    // Check if we can decrypt
    if disc.encrypted && disc.aacs.is_none() {
        println!();
        eprintln!("Cannot rip — disc is encrypted and no AACS keys found.");
        eprintln!("Provide KEYDB.cfg with --keydb or place at ~/.config/aacs/KEYDB.cfg");
        std::process::exit(1);
    }

    // Step 5: Select title (already computed target_idx above)
    let title = match disc.titles.get(target_idx) {
        Some(t) => t,
        None => {
            eprintln!("Title {} not found (have {})", target_idx + 1, disc.titles.len());
            std::process::exit(1);
        }
    };

    // Step 6: Output path — create directory if needed
    let out_dir = output_dir.map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("Cannot create output directory {}: {}", out_dir.display(), e);
        std::process::exit(1);
    }

    // Name from disc title: "Dune Part Two_t01.m2ts"
    let disc_name = disc.meta_title.as_deref()
        .unwrap_or(&disc.volume_id)
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != ' ' && c != '-', "")
        .trim().to_string();
    let filename = if disc_name.is_empty() {
        format!("title_{:05}.m2ts", title.playlist_id)
    } else {
        format!("{}_t{:02}.m2ts", disc_name, target_idx + 1)
    };
    let out_file = out_dir.join(&filename);

    println!();
    println!("Ripping title {} ({}) -> {}", target_idx + 1, title.duration_display(), out_file.display());
    println!("  {} extent(s), {:.1} GB", title.extents.len(), title.size_gb());

    if title.extents.is_empty() {
        eprintln!("No sector extents found for this title.");
        std::process::exit(1);
    }

    // Step 7: Install signal handler for clean interrupt
    install_signal_handler();

    // Step 8: Read and write
    let mut reader = match disc.open_title(&mut session, target_idx) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to open title: {}", e);
            std::process::exit(1);
        }
    };

    let total_bytes = reader.total_bytes();

    let outfile = match std::fs::File::create(&out_file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Cannot create {}: {}", out_file.display(), e);
            std::process::exit(1);
        }
    };
    let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, outfile); // 4MB write buffer

    let mut bytes_written = 0u64;
    let start = std::time::Instant::now();
    let mut last_progress = std::time::Instant::now();
    let mut last_bytes = 0u64;
    let mut peak_speed = 0.0f64;

    let mut interrupted = false;

    loop {
        // Check for interrupt or duration limit
        if INTERRUPTED.load(Ordering::Relaxed) {
            interrupted = true;
            break;
        }
        if let Some(max_secs) = duration_secs {
            if start.elapsed().as_secs() >= max_secs {
                interrupted = true;
                break;
            }
        }

        match reader.read_batch() {
            Ok(Some(batch)) => {
                let batch_len = batch.len() as u64;
                writer.write_all(batch).unwrap_or_else(|e| {
                    eprintln!("\nWrite error: {}", e);
                    std::process::exit(1);
                });
                bytes_written += batch_len;

                // Progress every ~2 seconds (more responsive than byte-based)
                let now = std::time::Instant::now();
                if now.duration_since(last_progress).as_secs_f64() >= 2.0 {
                    let elapsed = start.elapsed().as_secs_f64();
                    let mb = bytes_written as f64 / (1024.0 * 1024.0);
                    let avg_speed = mb / elapsed;

                    // Recent speed (since last update)
                    let interval = now.duration_since(last_progress).as_secs_f64();
                    let recent_mb = (bytes_written - last_bytes) as f64 / (1024.0 * 1024.0);
                    let recent_speed = recent_mb / interval;
                    if recent_speed > peak_speed { peak_speed = recent_speed; }

                    let pct = if total_bytes > 0 {
                        (bytes_written as f64 / total_bytes as f64 * 100.0).min(100.0)
                    } else {
                        0.0
                    };
                    let eta = if avg_speed > 0.0 && total_bytes > 0 {
                        let remaining_mb = (total_bytes - bytes_written) as f64 / (1024.0 * 1024.0);
                        let secs = remaining_mb / avg_speed;
                        format!("{}:{:02}", (secs / 60.0) as u32, (secs % 60.0) as u32)
                    } else {
                        "--:--".into()
                    };

                    let err_str = if reader.errors > 0 {
                        format!("  err:{}", reader.errors)
                    } else {
                        String::new()
                    };

                    eprint!("\r  {:.0} MB / {:.0} MB  ({:.0}%)  {:.1} MB/s (cur: {:.1})  ETA {}{}   ",
                        mb, total_bytes as f64 / (1024.0 * 1024.0), pct,
                        avg_speed, recent_speed, eta, err_str);

                    last_progress = now;
                    last_bytes = bytes_written;
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("\nRead error: {}", e);
            }
        }
    }

    let errors = reader.errors;

    // Flush remaining buffered data
    drop(reader); // release session borrow
    let _ = writer.flush();

    let elapsed = start.elapsed().as_secs_f64();
    let mb = bytes_written as f64 / (1024.0 * 1024.0);

    if interrupted {
        eprintln!("\n\nInterrupted — ejecting disc...");
        let _ = session.eject();
        println!("Partial: {:.0} MB in {:.0}s ({:.1} MB/s)", mb, elapsed, mb / elapsed);
        println!("Output:  {}", out_file.display());
        std::process::exit(130);
    }

    println!();
    println!();
    println!("Complete: {:.0} MB in {:.0}s", mb, elapsed);
    println!("Speed:    {:.1} MB/s avg, {:.1} MB/s peak", mb / elapsed, peak_speed);
    if errors > 0 {
        println!("Errors:   {} (sectors skipped)", errors);
    }
    println!("Output:   {}", out_file.display());
}

