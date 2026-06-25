// freemkv drive-info — Show drive information and capture profiles
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all drive data from libfreemkv.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::Drive;
use std::io::{IsTerminal, Write};
use std::path::Path;

pub fn run(device: Option<&str>, args: &[String]) {
    let mut share = false;
    let mut mask = false;
    let mut quiet = false;
    let mut verbose = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--share" | "-s" => share = true,
            "--mask" | "-m" => mask = true,
            "--quiet" | "-q" => quiet = true,
            "--verbose" | "-v" => verbose = true,
            // Log-level/log-file tokens are handled by main::init_logging;
            // accept them here so they aren't rejected as unknown options.
            "-vv" | "-vvv" => verbose = true,
            "--log-file" => {
                i += 1; // skip the path value
            }
            "--help" | "-h" => {
                println!("{}", strings::get("drive.share_usage"));
                println!();
                println!("  --share    {}", strings::get("drive.share_desc"));
                println!("  --mask     {}", strings::get("drive.mask_desc"));
                println!("  --quiet    {}", strings::get("app.opt_quiet"));
                println!("  --verbose  {}", strings::get("app.opt_verbose"));
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

    let mut session = match device {
        Some(p) => Drive::open(Path::new(p)).unwrap_or_else(|e| {
            eprintln!(
                "{}",
                strings::fmt(
                    "error.open_failed",
                    &[("device", p), ("error", &e.to_string())]
                )
            );
            std::process::exit(1);
        }),
        None => libfreemkv::find_drive().unwrap_or_else(|| {
            eprintln!("{}", strings::get("error.no_drive"));
            std::process::exit(1);
        }),
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
            session.device_path()
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

    // Build the profile dir name from the (untrusted) INQUIRY strings. These
    // come from drive firmware and could contain `/`, `\`, `..`, NUL, etc., so
    // sanitize to a strict allowlist before using the result as a path — a
    // malformed/malicious firmware string must not steer writes out of CWD.
    let profile_name = sanitize_component(&format!(
        "{}-{}-{}-{}",
        id.vendor_id.to_lowercase().trim(),
        id.product_id.to_lowercase().trim(),
        id.product_revision.to_lowercase().trim(),
        id.vendor_specific.to_lowercase().trim()
    ));

    let profile_dir = std::path::PathBuf::from(&profile_name);
    if let Err(e) = std::fs::create_dir_all(&profile_dir) {
        eprintln!(
            "{}",
            strings::fmt(
                "error.cannot_create_dir",
                &[
                    ("path", &profile_dir.display().to_string()),
                    ("error", &e.to_string())
                ]
            )
        );
        std::process::exit(1);
    }

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
        feat_lines.push(format!(
            "0x{:04X} = \"{}\"  # {}",
            feat.code, fname, feat.name
        ));
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
    // These fields are derived from raw INQUIRY / GET_CONFIG bytes (firmware-
    // controlled, `from_utf8_lossy`/`ascii_field`), so a value may contain a
    // double quote, backslash, or control char that would break the TOML
    // double-quoted string. Escape every embedded value.
    toml.push_str(&format!(
        "manufacturer = \"{}\"\n",
        toml_escape(id.vendor_id.trim())
    ));
    toml.push_str(&format!(
        "product = \"{}\"\n",
        toml_escape(id.product_id.trim())
    ));
    toml.push_str(&format!(
        "revision = \"{}\"\n",
        toml_escape(id.product_revision.trim())
    ));
    toml.push_str(&format!("serial = \"{}\"\n", toml_escape(&serial_toml)));
    toml.push_str(&format!(
        "firmware_date = \"{}\"\n",
        toml_escape(&format_date(&id.firmware_date))
    ));
    toml.push_str(&format!("platform = \"{}\"\n", toml_escape(&platform)));
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
    let toml_path = profile_dir.join("drive.toml");
    if let Err(e) = std::fs::write(&toml_path, &toml) {
        eprintln!("Cannot write {}: {}", toml_path.display(), e);
        std::process::exit(1);
    }

    // ── Summarize captured profile ─────────────────────────────────────────

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

    // ── Package + present for manual submission ────────────────────────────
    //
    // We zip the captured profile to disk and print a ready-to-paste GitHub
    // issue (title + body + the issues/new URL); the user submits it manually.
    // A genuine I/O failure (zip or write) exits non-zero so scripts can detect
    // it.

    print!("  {}  ", strings::get("drive.submit_packaging"));
    let _ = std::io::stdout().flush();
    let zip_data = match zip_directory(&profile_dir) {
        Ok(d) => d,
        Err(e) => {
            println!(
                "{}",
                strings::fmt("drive.zip_failed", &[("error", &e.to_string())])
            );
            std::process::exit(1);
        }
    };
    let zip_path = profile_dir.join("profile.zip");
    if let Err(e) = std::fs::write(&zip_path, &zip_data) {
        eprintln!(
            "{}",
            strings::fmt(
                "error.cannot_write",
                &[
                    ("path", &zip_path.display().to_string()),
                    ("error", &e.to_string())
                ]
            )
        );
        std::process::exit(1);
    }
    let zip_b64 = base64_encode(&zip_data);
    println!("{} bytes", zip_data.len());

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

    body.push_str("<details><summary>Profile data (base64 zip)</summary>\n\n");
    body.push_str("```\n");
    for chunk in zip_b64.as_bytes().chunks(76) {
        // base64 output is pure ASCII, so a 76-byte chunk is always valid UTF-8
        // on a char boundary; surface the impossible case loudly rather than
        // silently dropping a line of profile data.
        body.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        body.push('\n');
    }
    body.push_str("```\n\n");
    body.push_str("</details>\n\n");

    body.push_str("---\n*Captured by `freemkv drive-info --share`*\n");

    let title = format!(
        "Drive profile: {} {}",
        id.vendor_id.trim(),
        id.product_id.trim()
    );

    present_for_submission(&profile_name, &zip_path, &title, &body);

    // The captured profile (and its zip) are kept on disk so the user can
    // attach/paste them when filing the issue. Do NOT remove the dir.
}

/// Print everything the user needs to file the drive-profile issue by hand.
///
/// We print the issue title, the pre-filled new-issue URL, and the full issue
/// body, and point at the saved zip. The user pastes it into the issue. This
/// always exits cleanly — the artifact is on disk.
fn present_for_submission(profile_name: &str, zip_path: &Path, title: &str, body: &str) {
    println!();
    println!(
        "{}",
        strings::fmt("drive.submit_saved", &[("dir", profile_name)])
    );
    println!(
        "{}",
        strings::fmt(
            "drive.submit_zip",
            &[("path", &zip_path.display().to_string())]
        )
    );

    // Build-injected, issues-only PAT scoped to freemkv/bdemu. Set
    // FREEMKV_GH_TOKEN at build time (release CI / local) so the secret lives
    // in the binary, NOT the source — GitHub's secret scanner revokes any token
    // committed to a repo (that's what killed the old base64'd one). When no
    // token is compiled in, we fall straight through to the manual flow below.
    let token = option_env!("FREEMKV_GH_TOKEN").unwrap_or("").trim();
    // Only offer auto-submit on an INTERACTIVE terminal. A non-interactive stdin
    // (closed/redirected/piped — a CI runner, cron, or `... --share </dev/null`)
    // cannot give informed consent, and EOF there must NOT be read as "yes": the
    // posted profile carries the drive serial (unless --mask), so a default-yes
    // on EOF would upload identifying hardware data without the user agreeing.
    // In that case fall straight through to the manual flow below.
    if !token.is_empty() && std::io::stdin().is_terminal() {
        println!();
        eprint!("Submit this profile to help expand drive support? [Y/n] ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        let n = std::io::stdin().read_line(&mut input).unwrap_or(0);
        let ans = input.trim();
        // A bare Enter (n>0, empty after trim) is the [Y] default; EOF (n==0,
        // e.g. ^D or a stdin that closed despite is_terminal) is NOT consent.
        if n > 0 && (ans.is_empty() || ans.eq_ignore_ascii_case("y")) {
            match submit_issue(token, title, body) {
                Some(url) => {
                    println!();
                    println!("Submitted — thank you!");
                    println!("  {url}");
                    return;
                }
                None => {
                    println!();
                    println!("Automated submission failed; you can still file it by hand:");
                    // fall through to the manual instructions
                }
            }
        } else {
            println!("Not submitted. You can still file it by hand if you like:");
            // fall through to the manual instructions
        }
    }

    println!();
    println!("{}", strings::get("drive.submit_manual"));
    println!("  https://github.com/freemkv/bdemu/issues/new");
    println!();
    println!(
        "{}",
        strings::fmt("drive.submit_issue_title", &[("title", title)])
    );
    println!();
    println!("{}", strings::get("drive.submit_issue_body"));
    println!("────────────────────────────────────────");
    print!("{}", body);
    println!("────────────────────────────────────────");
}

/// POST a drive-profile issue to `freemkv/bdemu` via the GitHub Issues API.
///
/// Returns the issue's `html_url` on success, `None` on any failure (the caller
/// falls back to the manual print path). Uses `curl` to avoid pulling an HTTP
/// stack into the CLI, matching the original 0.1-era implementation.
fn submit_issue(token: &str, title: &str, body: &str) -> Option<String> {
    const REPO: &str = "freemkv/bdemu";
    let payload = format!(
        r#"{{"title":"{}","body":"{}","labels":["drive-profile"]}}"#,
        json_escape(title),
        json_escape(body)
    );

    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            &format!("https://api.github.com/repos/{REPO}/issues"),
            "-H",
            &format!("Authorization: token {token}"),
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: freemkv-info",
            "-d",
            &payload,
        ])
        .output()
        .ok()?;

    let response = String::from_utf8_lossy(&output.stdout);
    // Pull out "html_url":"…/issues/N" (skip the repo/user html_url fields).
    for (idx, _) in response.match_indices("\"html_url\":\"") {
        let rest = &response[idx + "\"html_url\":\"".len()..];
        if let Some(end) = rest.find('"') {
            let url = &rest[..end];
            if url.contains("/issues/") {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// Minimal JSON string escaper for the issue payload (quotes, backslashes,
/// newlines, and control chars). The body carries base64 + backticks, so a
/// naive replace isn't enough.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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
            // Skip our own output: a repeat `--share` in the same directory
            // would otherwise nest the previous run's profile.zip inside the new
            // archive.
            if name == "profile.zip" {
                continue;
            }
            zip.start_file(&name, options)?;
            let data = std::fs::read(entry.path())?;
            zip.write_all(&data)?;
        }
    }

    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

fn save_bin(dir: &std::path::Path, name: &str, data: &[u8]) {
    let path = dir.join(name);
    if let Err(e) = std::fs::write(&path, data) {
        eprintln!("Cannot write {}: {}", path.display(), e);
        std::process::exit(1);
    }
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

/// Reduce an untrusted firmware-derived string to a safe single path component:
/// lowercase ASCII alphanumerics, `-`, and `_` only; every other byte (spaces,
/// `/`, `\`, `.`, NUL, multibyte) becomes `-`; runs of `-` collapse to one; and
/// leading/trailing `-` are trimmed. The result can never be `.`, `..`, contain
/// a separator, or escape the working directory. Falls back to `drive` if empty.
fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        let keep = if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            true
        } else if c == '_' {
            out.push('_');
            true
        } else {
            // Collapse any run of disallowed chars into a single '-'.
            if !last_dash {
                out.push('-');
            }
            last_dash = true;
            continue;
        };
        if keep {
            last_dash = false;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "drive".to_string()
    } else {
        trimmed
    }
}

/// Escape a string for embedding inside a TOML basic (double-quoted) string.
///
/// The drive identity fields come from raw INQUIRY / GET_CONFIG bytes under
/// firmware control, so a value can legitimately contain a `"`, `\`, or a
/// control character (newline, NUL, etc.). Embedded verbatim those break the
/// `key = "..."` line and make `drive.toml` unparseable. Backslash and quote
/// are backslash-escaped; the TOML-defined control escapes (`\n`, `\r`, `\t`)
/// are emitted by name; every other C0 control / DEL becomes a `\uXXXX` escape.
/// Ordinary printable text passes through unchanged.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c.is_control()) => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn format_date(fw_date: &str) -> String {
    // The byte-index slices below are only sound on ASCII. `len()` is a byte
    // length, so a corrupted/non-ASCII firmware-date field (multibyte UTF-8
    // with a char boundary mid-slice) would panic. Guard on `is_ascii()` and
    // fall through to the raw passthrough for anything unexpected.
    if fw_date.len() < 8 || !fw_date.is_ascii() {
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

// Decoder is the inverse of `base64_encode`; its only consumer is the round-trip
// test that guards the encoder, so it is gated test-only and never compiled into
// the release binary.
#[cfg(test)]
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

/// Decode a TOML basic (double-quoted) string body — the inverse of
/// [`toml_escape`]. Test-only: it exists to prove the encoder's output is a
/// well-formed basic string that round-trips, without pulling in a full TOML
/// parser dependency. Panics on a malformed escape (which would mean the
/// encoder emitted something a real TOML parser would also reject).
#[cfg(test)]
fn toml_basic_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            // A raw control char or quote inside a basic-string body is invalid
            // TOML — the encoder must never produce one.
            assert!(
                c != '"' && !c.is_control(),
                "unescaped control/quote in basic string body: {c:?}"
            );
            out.push(c);
            continue;
        }
        match chars.next().expect("dangling escape") {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                let cp = u32::from_str_radix(&hex, 16).expect("bad \\u escape");
                out.push(char::from_u32(cp).expect("invalid scalar in \\u escape"));
            }
            other => panic!("unsupported escape \\{other}"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        base64_decode, base64_encode, format_date, hex_dump, json_escape, sanitize_component,
        toml_basic_unescape, toml_escape,
    };

    #[test]
    fn json_escape_handles_quotes_backslashes_control_chars() {
        // The auto-post issue body is base64 + backticks + newlines, so the
        // POST payload must be valid JSON. A naive replace would break on
        // backslashes and control chars.
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("line1\nline2"), "line1\\nline2");
        assert_eq!(json_escape("a\tb\r"), "a\\tb\\r");
        // A bare control char (e.g. 0x01) must become a \u escape, not a raw byte.
        assert_eq!(json_escape("\u{0001}"), "\\u0001");
        // Backticks are NOT special in JSON — must pass through untouched (the
        // body is full of them from the firmware hex dumps).
        assert_eq!(json_escape("`code`"), "`code`");
        // Combined: the result must round-trip as a valid JSON string body.
        let escaped = json_escape("he said \"hi\"\npath: C:\\x");
        let doc = format!("{{\"v\":\"{escaped}\"}}");
        let parsed: serde_json::Value = serde_json::from_str(&doc).expect("valid JSON");
        assert_eq!(parsed["v"], "he said \"hi\"\npath: C:\\x");
    }

    #[test]
    fn sanitize_component_blocks_path_traversal() {
        // Untrusted firmware strings must never escape CWD or become . / .. .
        assert!(!sanitize_component("../../etc/passwd").contains('/'));
        assert!(!sanitize_component("..\\..\\windows").contains('\\'));
        assert_ne!(sanitize_component(".."), "..");
        assert_ne!(sanitize_component("."), ".");
        // No path separators or NUL survive.
        for bad in ["a/b", "a\\b", "a\0b", "/abs", "lead/../x"] {
            let s = sanitize_component(bad);
            assert!(
                !s.contains('/') && !s.contains('\\') && !s.contains('\0'),
                "{s:?}"
            );
        }
    }

    #[test]
    fn sanitize_component_collapses_and_trims_dashes() {
        // Runs of disallowed chars collapse to a single '-' (fixes the old
        // single-pass "--"->"-" that left residual "--" on "---").
        assert_eq!(sanitize_component("a   b"), "a-b");
        assert_eq!(sanitize_component("a---b"), "a-b");
        assert_eq!(sanitize_component("a / / b"), "a-b");
        assert_eq!(sanitize_component("-lead-"), "lead");
        assert_eq!(sanitize_component("HL-DT-ST BD"), "hl-dt-st-bd");
        // Empty / all-bad input falls back to a safe default.
        assert_eq!(sanitize_component(""), "drive");
        assert_eq!(sanitize_component("///"), "drive");
        // Underscores and alphanumerics survive, lowercased.
        assert_eq!(sanitize_component("Foo_Bar1"), "foo_bar1");
    }

    #[test]
    fn toml_escape_round_trips_to_parseable_toml() {
        // Regression (HIGH): firmware INQUIRY/GET_CONFIG strings were embedded
        // into `drive.toml` double-quoted values with NO escaping, so a value
        // containing `"`, `\`, or a newline produced an unparseable file. Every
        // such value must escape to a well-formed basic string that decodes back
        // to the original.
        let cases = [
            r#"HL-DT-ST"#,          // ordinary
            r#"BAD"VENDOR"#,        // embedded quote
            r#"C:\firmware\v2"#,    // embedded backslashes
            "line1\nline2",         // embedded newline
            "tab\there\r\n",        // tab + CRLF
            "nul\0byte",            // NUL control char
            r#"both \ and " here"#, // both special chars
            "ünïcödé",              // multibyte printable passes through
        ];
        for raw in cases {
            let escaped = toml_escape(raw);
            // The escaped body must contain no raw quote, backslash-quote aside,
            // and no raw control characters — i.e. it is a valid basic-string body.
            assert!(
                !escaped.chars().any(|c| c == '\n' || c == '\r'),
                "escaped value still contains a raw newline: {escaped:?}"
            );
            // Build the actual line we emit and confirm it parses (manually) into
            // exactly the original value.
            let line = format!("manufacturer = \"{escaped}\"\n");
            let body = line
                .trim_end()
                .strip_prefix("manufacturer = \"")
                .and_then(|s| s.strip_suffix('"'))
                .expect("well-formed key = \"...\" line");
            assert_eq!(
                toml_basic_unescape(body),
                raw,
                "round-trip failed for {raw:?} (escaped {escaped:?})"
            );
        }
    }

    #[test]
    fn format_date_non_ascii_passes_through() {
        // Regression: byte-slicing a non-ASCII firmware date panicked. It must
        // fall through to the raw passthrough instead.
        let s = "20\u{00e9}1231"; // 'é' is multibyte; len()>=8 but not ASCII
        assert_eq!(format_date(s), s);
    }

    #[test]
    fn base64_encode_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_round_trips_arbitrary_lengths() {
        // Covers all three padding cases (len % 3 = 0/1/2) across many sizes.
        for len in 0..40usize {
            let data: Vec<u8> = (0..len)
                .map(|i| (i as u8).wrapping_mul(37).wrapping_add(11))
                .collect();
            assert_eq!(
                base64_decode(&base64_encode(&data)),
                data,
                "round-trip failed at len {len}"
            );
        }
    }

    #[test]
    fn format_date_standard_yyyymmdd() {
        assert_eq!(format_date("20211231"), "2021-12-31");
        assert_eq!(format_date("19991009"), "1999-10-09");
    }

    #[test]
    fn format_date_too_short_passes_through() {
        assert_eq!(format_date("2021"), "2021");
        assert_eq!(format_date(""), "");
    }

    #[test]
    fn hex_dump_formats_lowercase_and_wraps_at_32() {
        assert_eq!(hex_dump(&[0x00, 0x0f, 0xa0, 0xff]), "00 0f a0 ff");
        let data: Vec<u8> = (0..33u8).collect();
        let dump = hex_dump(&data);
        assert!(dump.contains('\n'), "should wrap after 32 bytes: {dump}");
        assert!(dump.starts_with("00 01 02"), "{dump}");
    }
}
