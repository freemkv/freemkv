// Speed benchmark — test SET_CD_SPEED strategies on a real drive.
// Usage: freemkv bench-speed [--device /dev/sgN]

use std::path::Path;
use std::time::Instant;
use libfreemkv::DriveSession;

const SECTORS_PER_READ: u16 = 48;
const BYTES_PER_SECTOR: usize = 2048;
const READ_SIZE: usize = SECTORS_PER_READ as usize * BYTES_PER_SECTOR;

/// Send SET_CD_SPEED with given read speed (KB/s).
fn set_speed(session: &mut DriveSession, speed_kbs: u16) {
    let cdb = libfreemkv::scsi::build_set_cd_speed(speed_kbs);
    let mut dummy = [0u8; 0];
    let _ = session.scsi_execute(
        &cdb,
        libfreemkv::scsi::DataDirection::None,
        &mut dummy,
        5_000,
    );
}

/// Read MODE SENSE page 2A and return (max_read_kbs, current_read_kbs).
fn read_speed_caps(session: &mut DriveSession) -> (u16, u16) {
    let cdb = [0x5A, 0x00, 0x2A, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00];
    let mut buf = [0u8; 256];
    if session.scsi_execute(
        &cdb,
        libfreemkv::scsi::DataDirection::FromDevice,
        &mut buf,
        5_000,
    ).is_err() {
        return (0, 0);
    }
    let pg = &buf[8..];
    let max_rd = u16::from_be_bytes([pg[8], pg[9]]);
    let cur_rd = u16::from_be_bytes([pg[10], pg[11]]);
    (max_rd, cur_rd)
}

/// Run a sustained read test with periodic speed reporting.
/// Reports speed every `report_interval` reads.
/// Optional: call set_speed every `speed_interval` reads (0 = never).
fn sustained_bench(
    session: &mut DriveSession,
    label: &str,
    start_lba: u32,
    total_reads: usize,
    speed_kbs: u16,       // 0 = don't send SET_CD_SPEED at all
    speed_interval: usize, // 0 = send once before, >0 = send every N reads
) {
    println!("{}:", label);

    // Send initial SET_CD_SPEED if requested
    if speed_kbs > 0 && speed_interval == 0 {
        set_speed(session, speed_kbs);
        println!("  SET_CD_SPEED {} sent once", speed_kbs);
    } else if speed_kbs > 0 {
        set_speed(session, speed_kbs);
        println!("  SET_CD_SPEED {} every {} reads", speed_kbs, speed_interval);
    } else {
        println!("  No SET_CD_SPEED");
    }

    let mut buf = vec![0u8; READ_SIZE];
    let start = Instant::now();
    let mut total_bytes = 0u64;
    let mut window_start = Instant::now();
    let mut window_bytes = 0u64;
    let mut errors = 0u32;

    for i in 0..total_reads {
        // Periodic SET_CD_SPEED
        if speed_kbs > 0 && speed_interval > 0 && i > 0 && i % speed_interval == 0 {
            set_speed(session, speed_kbs);
        }

        let lba = start_lba + (i as u32 * SECTORS_PER_READ as u32);
        match session.read_content(lba, SECTORS_PER_READ, &mut buf) {
            Ok(_) => {
                total_bytes += READ_SIZE as u64;
                window_bytes += READ_SIZE as u64;
            }
            Err(_) => { errors += 1; }
        }

        // Report every 5 seconds
        let window_elapsed = window_start.elapsed().as_secs_f64();
        if window_elapsed >= 5.0 {
            let window_speed = window_bytes as f64 / window_elapsed / 1_000_000.0;
            let overall_speed = total_bytes as f64 / start.elapsed().as_secs_f64() / 1_000_000.0;
            println!("  {:6.0} MB  current: {:5.1} MB/s  avg: {:5.1} MB/s  reads: {}",
                total_bytes as f64 / 1_000_000.0,
                window_speed,
                overall_speed,
                i + 1,
            );
            window_start = Instant::now();
            window_bytes = 0;
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let overall_speed = total_bytes as f64 / elapsed / 1_000_000.0;
    let (_, cur) = read_speed_caps(session);
    println!("  TOTAL: {:.0} MB in {:.1}s = {:.1} MB/s  errors: {}  mode_sense: {} KB/s",
        total_bytes as f64 / 1_000_000.0, elapsed, overall_speed, errors, cur);
    println!();
}

pub fn run(args: &[String]) {
    let mut device_path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--device" | "-d" => {
                i += 1;
                device_path = args.get(i).cloned();
            }
            _ => {}
        }
        i += 1;
    }

    let dev_path = device_path.unwrap_or_else(|| {
        libfreemkv::drive::find_drive().unwrap_or_else(|| {
            eprintln!("No BD drive found. Use --device /dev/sgN");
            std::process::exit(1);
        })
    });

    println!("freemkv bench-speed v2");
    println!();

    let mut session = match DriveSession::open(Path::new(&dev_path)) {
        Ok(s) => s,
        Err(e) => { eprintln!("Cannot open {}: {}", dev_path, e); std::process::exit(1); }
    };
    println!("Drive: {} {}", session.drive_id.vendor_id.trim(), session.drive_id.product_id.trim());

    if let Err(e) = session.wait_ready() {
        eprintln!("Drive not ready: {}", e);
        std::process::exit(1);
    }

    // Read capacity
    let cap_cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut cap_buf = [0u8; 8];
    let disc_sectors = if session.scsi_execute(
        &cap_cdb,
        libfreemkv::scsi::DataDirection::FromDevice,
        &mut cap_buf,
        5_000,
    ).is_ok() {
        u32::from_be_bytes([cap_buf[0], cap_buf[1], cap_buf[2], cap_buf[3]]) + 1
    } else {
        eprintln!("Cannot read disc capacity");
        std::process::exit(1);
    };
    println!("Disc:  {} sectors ({:.1} GB)", disc_sectors, disc_sectors as f64 * 2048.0 / 1e9);

    let (max_rd, cur_rd) = read_speed_caps(&mut session);
    println!("Max read speed:     {} KB/s ({:.1}x BD)", max_rd, max_rd as f64 / 4500.0);
    println!("Current read speed: {} KB/s ({:.1}x BD)", cur_rd, cur_rd as f64 / 4500.0);
    println!();

    // ~20000 reads = ~2 GB per test, enough to see full speed ramp
    let reads = 20000;

    // Start from content area (typical m2ts starts around here)
    let content_lba: u32 = 0x20000;

    // Test 1: No SET_CD_SPEED
    sustained_bench(&mut session, "Test 1: No SET_CD_SPEED", content_lba, reads, 0, 0);

    // Test 2: SET_CD_SPEED 0xFFFF once
    sustained_bench(&mut session, "Test 2: SET_CD_SPEED 0xFFFF once", content_lba, reads, 0xFFFF, 0);

    // Test 3: SET_CD_SPEED max (exact) once
    sustained_bench(&mut session, &format!("Test 3: SET_CD_SPEED {} (exact max) once", max_rd), content_lba, reads, max_rd, 0);

    // Test 4: SET_CD_SPEED 0xFFFF every 100 reads (~10 MB)
    sustained_bench(&mut session, "Test 4: SET_CD_SPEED 0xFFFF every 100 reads", content_lba, reads, 0xFFFF, 100);

    // Test 5: Different LBA — outer disc
    let outer_lba = disc_sectors * 3 / 4;
    sustained_bench(&mut session, "Test 5: Outer disc + SET_CD_SPEED 0xFFFF once", outer_lba, reads, 0xFFFF, 0);

    println!("Done.");
}
