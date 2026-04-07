// freemkv drive-info — Show drive information
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all drive data from libfreemkv.

use libfreemkv::DriveSession;
use std::path::Path;

pub fn run(args: &[String]) {
    let mut device_path: Option<String> = None;
    let mut share = false;
    let mut mask = false;
    let mut quiet = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--device" | "-d" => { i += 1; device_path = args.get(i).cloned(); }
            "--share" | "-s" => share = true,
            "--mask" | "-m" => mask = true,
            "--quiet" | "-q" => quiet = true,
            "--help" | "-h" => {
                println!("Usage: freemkv drive-info [--share] [--mask] [--device /dev/sgN]");
                return;
            }
            _ => { eprintln!("Unknown option: {}", args[i]); std::process::exit(1); }
        }
        i += 1;
    }

    let dev_path = device_path.unwrap_or_else(|| find_bd_drive().unwrap_or_else(|| {
        eprintln!("No BD drive found. Use --device /dev/sgN");
        std::process::exit(1);
    }));

    // Open drive via libfreemkv — no unlock needed for info
    let session = match DriveSession::open_no_unlock(Path::new(&dev_path)) {
        Ok(s) => s,
        Err(e) => { eprintln!("Cannot open {}: {}", dev_path, e); std::process::exit(1); }
    };

    let id = &session.drive_id;
    let profile = &session.profile;

    let serial_display = if mask { mask_str(&id.serial_number) } else { id.serial_number.clone() };
    let platform = format!("{:?}", profile.chipset);
    let fw_version = format!("{}/{}", id.product_revision.trim(), id.vendor_specific.trim());

    if !quiet {
        println!("freemkv {}", env!("CARGO_PKG_VERSION"));
        println!();
        println!("Drive Information");
        println!("  Device:              {}", dev_path);
        println!("  Manufacturer:        {}", id.vendor_id.trim());
        println!("  Product:             {}", id.product_id.trim());
        println!("  Revision:            {}", id.product_revision.trim());
        println!("  Serial number:       {}", serial_display);
        println!("  Firmware date:       {}", format_date(&id.firmware_date));
        println!();
        println!("Platform Information");
        println!("  Drive platform:      {}", platform);
        println!("  Firmware version:    {}", fw_version);
        println!();
        if !share {
            println!("Run 'freemkv drive-info --share' to help expand drive support.");
        }
    }

    if share {
        // TODO: profile capture and submission via lib API
        println!("Profile sharing not yet implemented in new CLI.");
    }
}

fn mask_str(s: &str) -> String {
    s.chars().map(|c| {
        if c.is_ascii_alphabetic() { 'A' }
        else if c.is_ascii_digit() { '0' }
        else { c }
    }).collect()
}

fn format_date(fw_date: &str) -> String {
    if fw_date.len() >= 8 {
        if fw_date.starts_with("21") && fw_date.len() >= 12 {
            format!("20{}-{}-{}", &fw_date[2..4], &fw_date[4..6], &fw_date[6..8])
        } else if fw_date.len() >= 8 {
            format!("{}-{}-{}", &fw_date[0..4], &fw_date[4..6], &fw_date[6..8])
        } else {
            fw_date.to_string()
        }
    } else {
        fw_date.to_string()
    }
}

fn find_bd_drive() -> Option<String> {
    for i in 0..16 {
        let path = format!("/dev/sg{}", i);
        if Path::new(&path).exists() {
            if let Ok(session) = DriveSession::open_no_unlock(Path::new(&path)) {
                let vendor = session.drive_id.vendor_id.trim().to_lowercase();
                if vendor.contains("hl-dt") || vendor.contains("pioneer") || vendor.contains("asus") {
                    return Some(path);
                }
            }
        }
    }
    None
}
