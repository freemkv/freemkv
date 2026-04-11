// freemkv drive-info — Show drive information and capture profiles
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all drive data from libfreemkv.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::DriveSession;
use std::io::Write;
use std::path::Path;

pub fn run(args: &[String]) {
    let mut device_path: Option<String> = None;
    let mut share = false;
    let mut mask = false;
    let mut quiet = false;
    let mut verbose = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--device" | "-d" => {
                i += 1;
                device_path = args.get(i).cloned();
            }
            "--share" | "-s" => share = true,
            "--mask" | "-m" => mask = true,
            "--quiet" | "-q" => quiet = true,
            "--verbose" | "-v" => verbose = true,
            "--help" | "-h" => {
                println!("{}", strings::get("drive.share_usage"));
                println!();
                println!("  --share    {}", strings::get("drive.share_desc"));
                println!("  --mask     {}", strings::get("drive.mask_desc"));
                println!("  --device   {}", strings::get("drive.device_desc"));
                return;
            }
            _ => {
                eprintln!(
                    "{}",
                    strings::fmt("app.unknown_option", &[("opt", &args[i])])
                );
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let dev_path = device_path.unwrap_or_else(|| {
        libfreemkv::find_drive().unwrap_or_else(|| {
            eprintln!("{}", strings::get("error.no_drive"));
            std::process::exit(1);
        })
    });

    let mut session = match DriveSession::open(Path::new(&dev_path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "{}",
                strings::fmt(
                    "error.open_failed",
                    &[("device", &dev_path), ("error", &e.to_string())]
                )
            );
            std::process::exit(1);
        }
    };

    let id = session.drive_id.clone();
    let serial_display = if mask {
        libfreemkv::mask_string(&id.serial_number)
    } else {
        id.serial_number.clone()
    };
    let platform = session.platform_name().to_string();
    let fw_version = format!(
        "{}/{}",
        id.product_revision.trim(),
        id.vendor_specific.trim()
    );
    let profile_status = if session.has_profile() {
        strings::get("drive.supported")
    } else {
        strings::get("drive.unknown")
    };

    let out = Output::new(verbose, quiet);

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);
    out.print(Normal, "drive.header");
    out.raw(
        Normal,
        &format!(
            "  {}:              {}",
            strings::get("drive.device"),
            dev_path
        ),
    );
    out.raw(
        Normal,
        &format!(
            "  {}:        {}",
            strings::get("drive.manufacturer"),
            id.vendor_id.trim()
        ),
    );
    out.raw(
        Normal,
        &format!(
            "  {}:             {}",
            strings::get("drive.product"),
            id.product_id.trim()
        ),
    );
    out.raw(
        Normal,
        &format!(
            "  {}:            {}",
            strings::get("drive.revision"),
            id.product_revision.trim()
        ),
    );
    out.raw(
        Normal,
        &format!(
            "  {}:       {}",
            strings::get("drive.serial"),
            serial_display
        ),
    );
    out.raw(
        Normal,
        &format!(
            "  {}:       {}",
            strings::get("drive.firmware_date"),
            format_date(&id.firmware_date)
        ),
    );
    out.blank(Normal);
    out.print(Normal, "drive.platform_header");
    out.raw(
        Normal,
        &format!("  {}:      {}", strings::get("drive.platform"), platform),
    );
    out.raw(
        Normal,
        &format!(
            "  {}:    {}",
            strings::get("drive.firmware_version"),
            fw_version
        ),
    );
    out.raw(
        Normal,
        &format!(
            "  {}:             {}",
            strings::get("drive.profile"),
            profile_status
        ),
    );
    out.blank(Normal);
    if !share {
        out.print(Normal, "drive.share_hint");
    }

    if !share {
        return;
    }

    // ── Capture raw drive data via library ─────────────────────────────────

    let capture = match libfreemkv::capture_drive_data(&mut session) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Capture failed: {}", e);
            std::process::exit(1);
        }
    };

    let profile_name = format!(
        "{}-{}-{}-{}",
        id.vendor_id.to_lowercase().trim(),
        id.product_id.to_lowercase().trim().replace(' ', "-"),
        id.product_revision.to_lowercase().trim(),
        id.vendor_specific.to_lowercase().trim()
    )
    .replace('/', "-")
    .replace("--", "-");

    let profile_dir = std::path::PathBuf::from(&profile_name);
    std::fs::create_dir_all(&profile_dir).expect("Cannot create profile directory");

    // Save raw INQUIRY
    save_bin(&profile_dir, "inquiry.bin", &capture.inquiry);

    // Save captured features
    let mut feat_lines = Vec::new();
    for feat in &capture.features {
        let mut feat_data = feat.data.clone();

        // Mask serial in GET_CONFIG 0108
        if feat.code == 0x0108 && mask && feat_data.len() > 4 {
            let masked = libfreemkv::mask_bytes(&feat_data[4..]);
            feat_data[4..4 + masked.len()].copy_from_slice(&masked);
        }

        let fname = format!("gc_{:04x}.bin", feat.code);
        save_bin(&profile_dir, &fname, &feat_data);
        feat_lines.push(format!("0x{:04X} = \"{}\"  # {}", feat.code, fname, feat.name));
        if !quiet {
            println!(
                "  {}",
                strings::fmt(
                    "drive.captured",
                    &[
                        ("code", &format!("{:04X}", feat.code)),
                        ("name", feat.name),
                        ("bytes", &feat_data.len().to_string()),
                    ]
                )
            );
        }
    }

    // Save READ_BUFFER 0xF1 (Pioneer)
    if let Some(ref data) = capture.rb_f1 {
        let mut data = data.clone();
        if mask && data.len() >= 12 {
            let masked = libfreemkv::mask_bytes(&data[0..12]);
            data[0..12].copy_from_slice(&masked);
        }
        save_bin(&profile_dir, "rb_f1.bin", &data);
    }

    // Save READ_BUFFER mode 6 (MTK)
    if let Some(ref data) = capture.rb_mode6 {
        save_bin(&profile_dir, "rb_mode6.bin", data);
    }

    // Save RPC state
    if let Some(ref data) = capture.rpc_state {
        save_bin(&profile_dir, "rpc_state.bin", data);
    }

    // Save MODE SENSE 2A
    if let Some(ref data) = capture.mode_2a {
        save_bin(&profile_dir, "mode_2a.bin", data);
    }

    // ── Generate drive.toml ────────────────────────────────────────────────

    let serial_toml = if mask {
        libfreemkv::mask_string(&id.serial_number)
    } else {
        id.serial_number.clone()
    };
    let mut toml = String::new();
    toml.push_str(&format!(
        "# {} {} {} — freemkv drive-info\n\n",
        id.vendor_id.trim(),
        id.product_id.trim(),
        id.product_revision.trim()
    ));
    toml.push_str("[drive]\n");
    toml.push_str(&format!("manufacturer = \"{}\"\n", id.vendor_id.trim()));
    toml.push_str(&format!("product = \"{}\"\n", id.product_id.trim()));
    toml.push_str(&format!("revision = \"{}\"\n", id.product_revision.trim()));
    toml.push_str(&format!("serial = \"{}\"\n", serial_toml));
    toml.push_str(&format!(
        "firmware_date = \"{}\"\n",
        format_date(&id.firmware_date)
    ));
    toml.push_str(&format!("platform = \"{}\"\n", platform));
    toml.push_str(&format!("profile_matched = {}\n\n", session.has_profile()));
    toml.push_str("[files]\n");
    toml.push_str("inquiry = \"inquiry.bin\"\n");
    toml.push_str("mode_2a = \"mode_2a.bin\"\n\n");
    toml.push_str("[features]\n");
    for line in &feat_lines {
        toml.push_str(line);
        toml.push('\n');
    }
    if capture.rb_f1.is_some() || capture.rb_mode6.is_some() {
        toml.push_str("\n[read_buffer]\n");
        if capture.rb_f1.is_some() {
            toml.push_str("0xF1 = \"rb_f1.bin\"\n");
        }
        if capture.rb_mode6.is_some() {
            toml.push_str("mode6 = \"rb_mode6.bin\"\n");
        }
    }
    std::fs::write(profile_dir.join("drive.toml"), &toml).expect("Cannot write drive.toml");

    // ── Confirm + submit ───────────────────────────────────────────────────

    println!();
    println!("{}:", strings::get("drive.submit_header"));
    println!(
        "  {}:    {} {} {}",
        strings::get("drive.submit_drive"),
        id.vendor_id.trim(),
        id.product_id.trim(),
        id.product_revision.trim()
    );
    println!(
        "  {}:   {}",
        strings::get("drive.submit_serial"),
        serial_toml
    );
    println!("  {}: {}", strings::get("drive.submit_platform"), platform);
    println!(
        "  {}: {}",
        strings::get("drive.submit_firmware"),
        fw_version
    );
    println!(
        "  {}:  {}",
        strings::get("drive.submit_profile"),
        profile_status
    );
    println!(
        "  {}: {} captured",
        strings::get("drive.submit_features"),
        feat_lines.len()
    );
    println!();

    eprint!("{}", strings::get("drive.submit_confirm"));
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap_or(0);
    if !input.trim().eq_ignore_ascii_case("y") {
        println!(
            "{}",
            strings::fmt("drive.submit_not_sent", &[("dir", &profile_name)])
        );
        return;
    }

    // Zip profile directory
    print!("  {}  ", strings::get("drive.submit_packaging"));
    let _ = std::io::stdout().flush();
    let zip_b64 = match zip_directory(&profile_dir) {
        Ok(zip_data) => {
            let encoded = base64_encode(&zip_data);
            println!("{} bytes", zip_data.len());
            Some(encoded)
        }
        Err(e) => {
            println!(
                "{}",
                strings::fmt("drive.zip_failed", &[("error", &e.to_string())])
            );
            None
        }
    };

    // Build issue body
    let mut body = String::new();
    body.push_str("## Drive Profile\n\n");
    body.push_str("```\n");
    body.push_str(&format!("Manufacturer:    {}\n", id.vendor_id.trim()));
    body.push_str(&format!("Product:         {}\n", id.product_id.trim()));
    body.push_str(&format!(
        "Revision:        {}\n",
        id.product_revision.trim()
    ));
    body.push_str(&format!("Serial:          {}\n", serial_toml));
    body.push_str(&format!(
        "Firmware date:   {}\n",
        format_date(&id.firmware_date)
    ));
    body.push_str(&format!("Platform:        {}\n", platform));
    body.push_str(&format!("Firmware:        {}\n", fw_version));
    body.push_str(&format!("Profile:         {}\n", profile_status));
    body.push_str("```\n\n");
    body.push_str(&format!("Features captured: {}\n\n", feat_lines.len()));

    // Inline raw identity data — readable without downloading the zip
    body.push_str("### Raw identity\n\n");
    body.push_str("```\n");
    body.push_str(&format!(
        "INQUIRY[4] (additional length): 0x{:02X}\n",
        if capture.inquiry.len() > 4 {
            capture.inquiry[4]
        } else {
            0
        }
    ));
    body.push_str(&format!(
        "INQUIRY ({} bytes):\n  {}\n",
        capture.inquiry.len(),
        hex_dump(&capture.inquiry)
    ));
    if !capture.gc_010c.is_empty() {
        body.push_str(&format!(
            "GET_CONFIG 010C ({} bytes):\n  {}\n",
            capture.gc_010c.len(),
            hex_dump(&capture.gc_010c)
        ));
    } else {
        body.push_str("GET_CONFIG 010C: not available\n");
    }
    body.push_str("```\n\n");

    if let Some(ref b64) = zip_b64 {
        body.push_str("<details><summary>Profile data (base64 zip)</summary>\n\n");
        body.push_str("```\n");
        for chunk in b64.as_bytes().chunks(76) {
            body.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            body.push('\n');
        }
        body.push_str("```\n\n");
        body.push_str("</details>\n\n");
    }

    body.push_str("---\n*Submitted by `freemkv drive-info --share`*\n");

    let title = format!(
        "Drive profile: {} {}",
        id.vendor_id.trim(),
        id.product_id.trim()
    );

    submit_issue(&title, &body);

    // Clean up temp profile dir
    let _ = std::fs::remove_dir_all(&profile_dir);
}

fn submit_issue(title: &str, body: &str) {
    // Bot token: issues-only, scoped to freemkv/freemkv.
    const BOT_TOKEN_B64: &str = "Z2l0aHViX3BhdF8xMUFBSUpERlkweHJyd3NBaXI1SUhwXzBMcVowWERYejhxdVR6QUQyUllQSEFHYnN0OTlzc0gzaXJnWDJFWXB3aldZUEZNUzdFN0FIQ2ZqcEpx";
    let bot_token = String::from_utf8(base64_decode(BOT_TOKEN_B64)).unwrap_or_default();

    let payload = serde_json::json!({
        "title": title,
        "body": body,
        "labels": ["drive-profile"]
    });

    print!("  {}  ", strings::get("drive.submit_sending"));
    let _ = std::io::stdout().flush();

    match ureq::post("https://api.github.com/repos/freemkv/freemkv/issues")
        .set("Authorization", &format!("token {}", bot_token))
        .set("Accept", "application/vnd.github.v3+json")
        .set("User-Agent", "freemkv")
        .send_json(&payload)
    {
        Ok(resp) => {
            if let Ok(json) = resp.into_json::<serde_json::Value>() as Result<serde_json::Value, _>
            {
                if let Some(url) = json["html_url"].as_str() {
                    println!("{}", strings::get("rip.ok"));
                    println!();
                    println!("{}", strings::get("drive.submit_ok"));
                    println!("{}", url);
                    return;
                }
            }
            println!("{}", strings::get("rip.failed"));
            eprintln!("{}", strings::get("drive.submit_failed"));
            eprintln!("  https://github.com/freemkv/freemkv/issues/new");
        }
        Err(e) => {
            println!("{}", strings::get("rip.failed"));
            eprintln!(
                "{}",
                strings::fmt("drive.submit_net_error", &[("error", &e.to_string())])
            );
            eprintln!("  https://github.com/freemkv/freemkv/issues/new");
        }
    }
}

fn zip_directory(dir: &std::path::Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::io::Cursor;
    let buf = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            zip.start_file(&name, options)?;
            let data = std::fs::read(entry.path())?;
            zip.write_all(&data)?;
        }
    }

    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

fn save_bin(dir: &std::path::Path, name: &str, data: &[u8]) {
    std::fs::write(dir.join(name), data).unwrap_or_else(|_| panic!("Cannot write {}", name));
}

fn hex_dump(data: &[u8]) -> String {
    data.chunks(32)
        .map(|chunk| {
            chunk
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n  ")
}

fn format_date(fw_date: &str) -> String {
    if fw_date.len() < 8 {
        return fw_date.to_string();
    }
    if fw_date.starts_with("21") && fw_date.len() >= 12 {
        format!("20{}-{}-{}", &fw_date[2..4], &fw_date[4..6], &fw_date[6..8])
    } else {
        format!("{}-{}-{}", &fw_date[0..4], &fw_date[4..6], &fw_date[6..8])
    }
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(input: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in input.as_bytes() {
        if b == b'=' {
            break;
        }
        let val = match TABLE.iter().position(|&c| c == b) {
            Some(v) => v as u32,
            None => continue,
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}
