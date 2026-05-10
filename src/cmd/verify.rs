// freemkv verify — Read every sector of a disc and report bad/slow/recovered
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all logic in libfreemkv::verify. This file only formats output.

use crate::strings;
use crate::style;

pub(crate) fn run(args: &[String]) {
    let url = args.first().map(|s| s.as_str()).unwrap_or("disc://");
    let parsed = libfreemkv::parse_url(url);

    let device = match &parsed {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => p.clone(),
        libfreemkv::StreamUrl::Disc { device: None } => match libfreemkv::find_drive() {
            Some(d) => std::path::PathBuf::from(d.device_path()),
            None => {
                eprintln!("{}", strings::get("error.no_drive"));
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("{}", strings::get("verify.disc_only"));
            std::process::exit(1);
        }
    };

    println!(
        "{}\n",
        style::dim(&format!("freemkv {}", env!("CARGO_PKG_VERSION")))
    );

    // Open and scan
    eprint!("{}", strings::get("verify.opening_drive"));
    let mut drive = match libfreemkv::Drive::open(&device) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}\n{}", style::warn(&strings::get("rip.failed")), e);
            std::process::exit(1);
        }
    };
    let _ = drive.wait_ready();
    let _ = drive.init();
    eprintln!("{}", style::ok(&strings::get("rip.ok")));

    eprint!("{}", strings::get("disc.scanning"));
    let scan_opts = libfreemkv::ScanOptions::default();
    let disc = match libfreemkv::Disc::scan(&mut drive, &scan_opts) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}\n{}", style::warn(&strings::get("rip.failed")), e);
            std::process::exit(1);
        }
    };
    if disc.titles.is_empty() {
        eprintln!("{}", strings::get("disc.no_titles"));
        std::process::exit(1);
    }
    let title = &disc.titles[0];
    let disc_name = disc.meta_title.as_deref().unwrap_or(&disc.volume_id);
    let total_sectors: u64 = title.extents.iter().map(|e| e.sector_count as u64).sum();
    let total_gb = total_sectors as f64 * 2048.0 / 1_073_741_824.0;
    eprintln!(
        "{}",
        strings::fmt(
            "verify.disc_summary",
            &[
                ("name", &style::hl(disc_name)),
                ("size", &format!("{:.1}", total_gb)),
                ("sectors", &total_sectors.to_string()),
            ]
        )
    );

    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());
    let _ = drive.probe_disc();

    eprintln!("\n{}", strings::get("verify.verifying"));
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
    let count_line = |label: String, count: u64| {
        format!(
            "  {:<12} {:>12}  ({:.4}%)",
            format!("{}:", label),
            count,
            count as f64 / result.total_sectors as f64 * 100.0
        )
    };
    println!();
    println!(
        "{}",
        style::hl(&format!("{}:", strings::get("verify.results_header")))
    );
    println!(
        "{}",
        count_line(strings::get("verify.count_good"), result.good)
    );
    if result.slow > 0 {
        println!(
            "{}",
            count_line(strings::get("verify.count_slow"), result.slow)
        );
    }
    if result.recovered > 0 {
        println!(
            "{}",
            count_line(strings::get("verify.count_recovered"), result.recovered)
        );
    }
    if result.bad > 0 {
        println!(
            "{}",
            count_line(strings::get("verify.count_bad"), result.bad)
        );
    }

    if !result.ranges.is_empty() {
        println!();
        for range in &result.ranges {
            let status_str = match range.status {
                libfreemkv::verify::SectorStatus::Slow => strings::get("verify.status_slow"),
                libfreemkv::verify::SectorStatus::Recovered => {
                    strings::get("verify.status_recovered")
                }
                libfreemkv::verify::SectorStatus::Bad => strings::get("verify.status_bad"),
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
                    format!(
                        " — {}",
                        strings::fmt(
                            "verify.chapter_label",
                            &[
                                ("ch", &ch.to_string()),
                                ("m", &format!("{:02}", m)),
                                ("s", &format!("{:02}", s)),
                            ]
                        )
                    )
                }
                None => String::new(),
            };
            // Color the status word per severity: bad → warn (yellow),
            // slow/recovered → hl (teal). Same affordance as the
            // damage marker on the rip progress line.
            let status_styled = match range.status {
                libfreemkv::verify::SectorStatus::Bad => style::warn(&status_str),
                _ => style::hl(&status_str),
            };
            let sectors_word = strings::get("verify.sectors_word");
            println!(
                "  {} {} {}-{} ({:.1} GB{}): {} {}",
                status_styled,
                sectors_word,
                range.start_lba,
                range.start_lba + range.count,
                gb,
                ch_str,
                range.count,
                sectors_word,
            );
        }
    }

    let elapsed = result.elapsed_secs;
    let m = elapsed as u32 / 60;
    let s = elapsed as u32 % 60;
    println!();
    let verdict_line = strings::fmt(
        "verify.verdict_format",
        &[
            ("pct", &format!("{:.4}", result.readable_pct())),
            ("m", &m.to_string()),
            ("s", &format!("{:02}", s)),
        ],
    );
    println!(
        "{} {}",
        style::hl(&format!("{}:", strings::get("verify.verdict_header"))),
        verdict_line,
    );

    if result.is_perfect() {
        println!("         {}", style::ok(&strings::get("verify.perfect")));
    } else if result.bad > 0 {
        let cluster_count = result
            .ranges
            .iter()
            .filter(|r| r.status == libfreemkv::verify::SectorStatus::Bad)
            .count();
        println!(
            "         {}",
            style::warn(&strings::fmt(
                "verify.bad_clusters",
                &[
                    ("count", &result.bad.to_string()),
                    ("clusters", &cluster_count.to_string()),
                ]
            ))
        );
    }

    std::process::exit(if result.bad > 0 { 1 } else { 0 });
}
