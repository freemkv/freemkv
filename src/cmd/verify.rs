// freemkv verify — Read every sector of a disc and report bad/slow/recovered
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all logic in libfreemkv::verify. This file only formats output.

pub fn run(args: &[String]) {
    let url = args.first().map(|s| s.as_str()).unwrap_or("disc://");
    let parsed = libfreemkv::parse_url(url);

    let device = match &parsed {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => p.clone(),
        libfreemkv::StreamUrl::Disc { device: None } => match libfreemkv::find_drive() {
            Some(d) => std::path::PathBuf::from(d.device_path()),
            None => {
                eprintln!("No drive found");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("verify only works with disc:// URLs");
            std::process::exit(1);
        }
    };

    println!("freemkv {}\n", env!("CARGO_PKG_VERSION"));

    // Open and scan
    eprint!("Opening drive...");
    let mut drive = match libfreemkv::Drive::open(&device) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("FAILED\n{}", e);
            std::process::exit(1);
        }
    };
    let _ = drive.wait_ready();
    let _ = drive.init();
    eprintln!("OK");

    eprint!("Scanning...");
    let scan_opts = libfreemkv::ScanOptions::default();
    let disc = match libfreemkv::Disc::scan(&mut drive, &scan_opts) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("FAILED\n{}", e);
            std::process::exit(1);
        }
    };
    if disc.titles.is_empty() {
        eprintln!("No titles found");
        std::process::exit(1);
    }
    let title = &disc.titles[0];
    let disc_name = disc.meta_title.as_deref().unwrap_or(&disc.volume_id);
    let total_sectors: u64 = title.extents.iter().map(|e| e.sector_count as u64).sum();
    let total_gb = total_sectors as f64 * 2048.0 / 1_073_741_824.0;
    eprintln!(
        "{} ({:.1} GB, {} sectors)",
        disc_name, total_gb, total_sectors
    );

    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());
    let _ = drive.probe_disc();

    eprintln!("\nVerifying...");
    let start = std::time::Instant::now();
    let last_print = std::sync::Mutex::new(std::time::Instant::now());

    let result = libfreemkv::verify::verify_title(
        &mut drive,
        title,
        batch,
        Some(&|p: &libfreemkv::progress::PassProgress| {
            let mut lp = last_print.lock().unwrap();
            if lp.elapsed().as_secs_f64() >= 1.0 || p.work_done == p.work_total {
                let pct = if p.work_total > 0 {
                    p.work_done * 100 / p.work_total
                } else {
                    0
                };
                let elapsed = start.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    p.bytes_good_total as f64 / (1024.0 * 1024.0) / elapsed
                } else {
                    0.0
                };
                eprint!(
                    "\r  {}% · {:.1} MB/s · {} / {} sectors",
                    pct, speed, p.work_done, p.work_total
                );
                *lp = std::time::Instant::now();
            }
            true // continue
        }),
    );
    eprintln!(); // newline after progress

    // Results
    println!();
    println!("Results:");
    println!(
        "  Good:        {:>12}  ({:.4}%)",
        result.good,
        result.good as f64 / result.total_sectors as f64 * 100.0
    );
    if result.slow > 0 {
        println!(
            "  Slow:        {:>12}  ({:.4}%)",
            result.slow,
            result.slow as f64 / result.total_sectors as f64 * 100.0
        );
    }
    if result.recovered > 0 {
        println!(
            "  Recovered:   {:>12}  ({:.4}%)",
            result.recovered,
            result.recovered as f64 / result.total_sectors as f64 * 100.0
        );
    }
    if result.bad > 0 {
        println!(
            "  Bad:         {:>12}  ({:.4}%)",
            result.bad,
            result.bad as f64 / result.total_sectors as f64 * 100.0
        );
    }

    if !result.ranges.is_empty() {
        println!();
        for range in &result.ranges {
            let status_str = match range.status {
                libfreemkv::verify::SectorStatus::Slow => "SLOW",
                libfreemkv::verify::SectorStatus::Recovered => "RECOVERED",
                libfreemkv::verify::SectorStatus::Bad => "BAD",
                _ => continue,
            };
            let gb = range.byte_offset as f64 / 1_073_741_824.0;
            let chapter_info = libfreemkv::verify::VerifyResult::chapter_at_offset(
                &title.chapters,
                range.byte_offset,
                title.duration_secs,
                title.size_bytes,
            );
            let ch_str = match chapter_info {
                Some((ch, secs)) => {
                    let m = secs as u32 / 60;
                    let s = secs as u32 % 60;
                    format!(" — Chapter {}, {:02}:{:02}", ch, m, s)
                }
                None => String::new(),
            };
            println!(
                "  {} sectors {}-{} ({:.1} GB{}): {} sectors",
                status_str,
                range.start_lba,
                range.start_lba + range.count,
                gb,
                ch_str,
                range.count
            );
        }
    }

    let elapsed = result.elapsed_secs;
    let m = elapsed as u32 / 60;
    let s = elapsed as u32 % 60;
    println!();
    println!(
        "Verdict: {:.4}% readable in {}:{:02}",
        result.readable_pct(),
        m,
        s
    );

    if result.is_perfect() {
        println!("         Disc is perfect.");
    } else if result.bad > 0 {
        println!(
            "         {} unrecoverable sectors in {} cluster(s).",
            result.bad,
            result
                .ranges
                .iter()
                .filter(|r| r.status == libfreemkv::verify::SectorStatus::Bad)
                .count()
        );
    }

    std::process::exit(if result.bad > 0 { 1 } else { 0 });
}
