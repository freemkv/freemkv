//! freemkv rip — Back up a disc.
//!
//! Opens the drive, scans the disc (UDF, playlists, AACS — all automatic),
//! shows what's on it, and backs up selected titles.

use std::path::{Path, PathBuf};

pub fn run(args: &[String]) {
    let mut device_path: Option<String> = None;
    let mut output_dir: Option<String> = None;
    let mut keydb_path: Option<String> = None;
    let mut title_idx: Option<usize> = None;
    let mut list_only = false;

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
                title_idx = args.get(i).and_then(|s| s.parse().ok());
            }
            "--list" | "-l" => {
                list_only = true;
            }
            _ => {
                if device_path.is_none() && args[i].starts_with("/dev/") {
                    device_path = Some(args[i].clone());
                }
            }
        }
        i += 1;
    }

    // Auto-detect device
    let device = device_path.unwrap_or_else(|| auto_detect_device());
    if device.is_empty() {
        eprintln!("No optical drive found. Specify with --device /dev/sr0");
        std::process::exit(1);
    }

    println!("freemkv rip v{}", env!("CARGO_PKG_VERSION"));
    println!();

    // Step 1: Open drive
    print!("Opening {}... ", device);
    let mut session = match libfreemkv::DriveSession::open(Path::new(&device)) {
        Ok(s) => {
            println!("OK");
            s
        }
        Err(e) => {
            println!("FAILED");
            eprintln!("  {}", e);
            std::process::exit(1);
        }
    };
    println!("  {} {}", session.drive_id.vendor_id.trim(), session.drive_id.product_id.trim());

    // Step 2: Scan disc (UDF + playlists + AACS — all automatic)
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

    // Step 3: Show disc info
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

    for (i, title) in disc.titles.iter().enumerate() {
        let marker = if Some(i) == title_idx { ">" } else { " " };
        println!("{} {:2}. {} — {:.1} GB — {} clip(s) — {}",
            marker, i, title.duration_display(), title.size_gb(),
            title.clips.len(), title.playlist);

        if !title.streams.is_empty() {
            for stream in &title.streams {
                println!("       {} {}", stream.kind_name(), stream.display());
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

    // Step 4: Select title
    let target_idx = title_idx.unwrap_or(0);
    let title = match disc.titles.get(target_idx) {
        Some(t) => t,
        None => {
            eprintln!("Title {} not found (have {})", target_idx, disc.titles.len());
            std::process::exit(1);
        }
    };

    // Step 5: Output path
    let out_dir = output_dir.map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });
    let out_file = out_dir.join(format!("title_{:05}.m2ts", title.playlist_id));

    println!();
    println!("Ripping title {} ({}) -> {}", target_idx, title.duration_display(), out_file.display());
    println!("  {} extents, {:.1} GB", title.extents.len(), title.size_gb());

    if title.extents.is_empty() {
        eprintln!("No sector extents found for this title.");
        std::process::exit(1);
    }

    // Step 6: Read and write
    let mut reader = match disc.open_title(&mut session, target_idx) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to open title: {}", e);
            std::process::exit(1);
        }
    };

    let mut outfile = match std::fs::File::create(&out_file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Cannot create {}: {}", out_file.display(), e);
            std::process::exit(1);
        }
    };

    use std::io::Write;
    let mut units_read = 0u64;
    let mut bytes_written = 0u64;
    let start = std::time::Instant::now();

    loop {
        match reader.read_unit() {
            Ok(Some(unit)) => {
                outfile.write_all(&unit).unwrap_or_else(|e| {
                    eprintln!("\nWrite error: {}", e);
                    std::process::exit(1);
                });
                units_read += 1;
                bytes_written += unit.len() as u64;

                // Progress every 1000 units (~6MB)
                if units_read % 1000 == 0 {
                    let elapsed = start.elapsed().as_secs_f64();
                    let mb = bytes_written as f64 / (1024.0 * 1024.0);
                    let speed = mb / elapsed;
                    eprint!("\r  {:.0} MB ({:.1} MB/s)  ", mb, speed);
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("\nRead error at unit {}: {}", units_read, e);
                // Continue on read errors (skip bad sectors)
                units_read += 1;
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let mb = bytes_written as f64 / (1024.0 * 1024.0);

    println!();
    println!();
    println!("Complete: {:.0} MB in {:.0}s ({:.1} MB/s)", mb, elapsed, mb / elapsed);
    println!("Output: {}", out_file.display());
}

fn auto_detect_device() -> String {
    // Try common Linux device paths
    for dev in &["/dev/sr0", "/dev/sr1", "/dev/sg4", "/dev/sg5"] {
        if Path::new(dev).exists() {
            return dev.to_string();
        }
    }
    String::new()
}
