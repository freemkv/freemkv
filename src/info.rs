// freemkv info disc:// — Show drive information and capture profiles
// MIT — freemkv project. CLI is dumb — all drive data from libfreemkv.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::Drive;
use std::io::{IsTerminal, Write};
use std::path::Path;

// The invocation printed into artifacts that leave this machine (TOML header,
// issue body). See docs/info.md — CAPTURE_COMMAND.
const CAPTURE_COMMAND: &str = "freemkv info disc://";

// The drive identity, as it is safe to put in front of a human: fields are
// sanitised ONCE here so no later site can print a raw firmware string.
// See docs/info.md — DriveIdentity.
struct DriveIdentity {
    vendor: String,
    product: String,
    revision: String,
    vendor_specific: String,
    serial: String,
    /// Still the raw `CCYYMMDDHHMI` field; [`format_date`] renders it, and its
    /// fallback is a verbatim passthrough, so it is sanitised here too.
    firmware_date: String,
}

impl DriveIdentity {
    /// Sanitise, then trim — in that order, because stripping a control
    /// character can expose the whitespace that was hiding behind it.
    fn field(s: &str) -> String {
        crate::disc_info::sanitize(s).trim().to_string()
    }

    fn from_drive(id: &libfreemkv::DriveId) -> Self {
        Self {
            vendor: Self::field(&id.vendor_id),
            product: Self::field(&id.product_id),
            revision: Self::field(&id.product_revision),
            vendor_specific: Self::field(&id.vendor_specific),
            serial: Self::field(&id.serial_number),
            firmware_date: Self::field(&id.firmware_date),
        }
    }

    /// `revision/vendor_specific` — the "Firmware" line and TOML/issue field.
    fn firmware_version(&self) -> String {
        format!("{}/{}", self.revision, self.vendor_specific)
    }

    /// The serial as it may be shown: `--mask` replaces the characters, but the
    /// masking is applied to the SANITISED value, never the raw one.
    fn serial_display(&self, mask: bool) -> String {
        if mask {
            freemkv_engine::mask_string(&self.serial)
        } else {
            self.serial.clone()
        }
    }
}

// The drive-identity block `freemkv info disc://` prints, as lines. Sanitises
// the raw `DriveId` itself. See docs/info.md — drive_identity_lines.
fn drive_identity_lines(raw: &libfreemkv::DriveId, device: &str, mask: bool) -> Vec<String> {
    let id = DriveIdentity::from_drive(raw);
    vec![
        format!(
            "  {}:              {}",
            strings::get("drive.device"),
            device
        ),
        format!(
            "  {}:        {}",
            strings::get("drive.manufacturer"),
            id.vendor
        ),
        format!(
            "  {}:             {}",
            strings::get("drive.product"),
            id.product
        ),
        format!(
            "  {}:            {}",
            strings::get("drive.revision"),
            id.revision
        ),
        format!(
            "  {}:       {}",
            strings::get("drive.serial"),
            id.serial_display(mask)
        ),
        format!(
            "  {}:       {}",
            strings::get("drive.firmware_date"),
            format_date(&id.firmware_date)
        ),
    ]
}

// The `drive.toml` header comment, and the blank line after it. NOT
// `toml_escape`d (a comment needs no escaping), safe only because
// `DriveIdentity` already stripped control chars. See docs/info.md.
fn toml_header_comment(id: &DriveIdentity) -> String {
    format!(
        "# {} {} {} — {CAPTURE_COMMAND}\n\n",
        id.vendor, id.product, id.revision
    )
}

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

    // The identity block for every later use — display, `drive.toml`, and the
    // shared issue body — sanitised once, here. See [`DriveIdentity`].
    let raw_id = session.drive_id.clone();
    let id = DriveIdentity::from_drive(&raw_id);
    let platform = session.platform_name().to_string();
    let fw_version = id.firmware_version();
    let profile_status = if session.has_profile() {
        strings::get("drive.supported")
    } else {
        strings::get("drive.unknown")
    };

    let out = Output::new(verbose, quiet);

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);
    out.print(Normal, "drive.header");
    for line in drive_identity_lines(&raw_id, session.device_path(), mask) {
        out.raw(Normal, &line);
    }
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

    let capture = match freemkv_engine::capture_drive_data(&mut session) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{}",
                strings::fmt(
                    "error.capture_failed",
                    &[("error", &crate::pipe::fmt_err(&e))]
                )
            );
            std::process::exit(1);
        }
    };

    // Profile dir name comes from untrusted firmware INQUIRY strings (may hold
    // `/`, `\`, `..`, NUL, etc.) — sanitize to a strict allowlist first so a
    // malicious firmware string can't steer writes out of CWD.
    let profile_name = sanitize_component(&format!(
        "{}-{}-{}-{}",
        id.vendor.to_lowercase(),
        id.product.to_lowercase(),
        id.revision.to_lowercase(),
        id.vendor_specific.to_lowercase()
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

    // Every file this run writes, in order. Only these are archived — see
    // `zip_files` for why walking the directory is not safe here.
    let mut written: Vec<String> = Vec::new();

    // Save raw INQUIRY
    save_bin(&profile_dir, "inquiry.bin", &capture.inquiry, &mut written);

    // Save captured features
    let mut feat_lines = Vec::new();
    for feat in &capture.features {
        let mut feat_data = feat.data.clone();

        // Mask serial in GET_CONFIG 0108
        if feat.code == 0x0108 && mask && feat_data.len() > 4 {
            let masked = freemkv_engine::mask_bytes(&feat_data[4..]);
            feat_data[4..4 + masked.len()].copy_from_slice(&masked);
        }

        let fname = format!("gc_{:04x}.bin", feat.code);
        save_bin(&profile_dir, &fname, &feat_data, &mut written);
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
            let masked = freemkv_engine::mask_bytes(&data[0..12]);
            data[0..12].copy_from_slice(&masked);
        }
        save_bin(&profile_dir, "rb_f1.bin", &data, &mut written);
    }

    // Save READ_BUFFER mode 6 (MTK)
    if let Some(ref data) = capture.rb_mode6 {
        save_bin(&profile_dir, "rb_mode6.bin", data, &mut written);
    }

    // Renesas/Pioneer vendor buffers, before/knock/after: rb_b0_* are the two
    // windows read pre-knock, wb_41 is the enable knock, *_postknock are those
    // windows post-knock (rb_f4 = 0xF4 window). Diff to see what the knock frees.
    if let Some(ref data) = capture.rb_b0_04 {
        save_bin(&profile_dir, "rb_b0_04.bin", data, &mut written);
    }
    if let Some(ref data) = capture.rb_b0_500000 {
        save_bin(&profile_dir, "rb_b0_500000.bin", data, &mut written);
    }
    if let Some(ref data) = capture.rb_f4 {
        save_bin(&profile_dir, "rb_f4.bin", data, &mut written);
    }
    if let Some(ref data) = capture.wb_41 {
        save_bin(&profile_dir, "wb_41.bin", data, &mut written);
    }
    if let Some(ref data) = capture.rb_b0_04_postknock {
        save_bin(&profile_dir, "rb_b0_04_postknock.bin", data, &mut written);
    }
    if let Some(ref data) = capture.rb_b0_500000_postknock {
        save_bin(
            &profile_dir,
            "rb_b0_500000_postknock.bin",
            data,
            &mut written,
        );
    }

    // Save RPC state
    if let Some(ref data) = capture.rpc_state {
        save_bin(&profile_dir, "rpc_state.bin", data, &mut written);
    }

    // Save MODE SENSE 2A
    if let Some(ref data) = capture.mode_2a {
        save_bin(&profile_dir, "mode_2a.bin", data, &mut written);
    }

    // ── Generate drive.toml ────────────────────────────────────────────────

    let serial_toml = id.serial_display(mask);
    let mut toml = String::new();
    toml.push_str(&toml_header_comment(&id));
    toml.push_str("[drive]\n");
    // These fields come from raw firmware-controlled INQUIRY/GET_CONFIG bytes and
    // may contain a quote, backslash, or control char that would break the TOML
    // double-quoted string — escape every embedded value.
    toml.push_str(&format!("manufacturer = \"{}\"\n", toml_escape(&id.vendor)));
    toml.push_str(&format!("product = \"{}\"\n", toml_escape(&id.product)));
    toml.push_str(&format!("revision = \"{}\"\n", toml_escape(&id.revision)));
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
    if capture.rb_f1.is_some()
        || capture.rb_mode6.is_some()
        || capture.rb_b0_04.is_some()
        || capture.rb_b0_500000.is_some()
        || capture.wb_41.is_some()
        || capture.rb_b0_04_postknock.is_some()
        || capture.rb_b0_500000_postknock.is_some()
        || capture.rb_f4.is_some()
    {
        toml.push_str("\n[read_buffer]\n");
        if capture.rb_f1.is_some() {
            toml.push_str("0xF1 = \"rb_f1.bin\"\n");
        }
        if capture.rb_mode6.is_some() {
            toml.push_str("mode6 = \"rb_mode6.bin\"\n");
        }
        if capture.rb_b0_04.is_some() {
            toml.push_str("0xB0_04 = \"rb_b0_04.bin\"\n");
        }
        if capture.rb_b0_500000.is_some() {
            toml.push_str("0xB0_500000 = \"rb_b0_500000.bin\"\n");
        }
        if capture.wb_41.is_some() {
            toml.push_str("0x41 = \"wb_41.bin\"\n");
        }
        if capture.rb_b0_04_postknock.is_some() {
            toml.push_str("0xB0_04_postknock = \"rb_b0_04_postknock.bin\"\n");
        }
        if capture.rb_b0_500000_postknock.is_some() {
            toml.push_str("0xB0_500000_postknock = \"rb_b0_500000_postknock.bin\"\n");
        }
        if capture.rb_f4.is_some() {
            toml.push_str("0xF4 = \"rb_f4.bin\"\n");
        }
    }
    let toml_path = profile_dir.join("drive.toml");
    if let Err(e) = std::fs::write(&toml_path, &toml) {
        eprintln!(
            "{}",
            strings::fmt(
                "error.cannot_write",
                &[
                    ("path", &toml_path.display().to_string()),
                    ("error", &e.to_string())
                ]
            )
        );
        std::process::exit(1);
    }
    written.push("drive.toml".to_string());

    // ── Summarize captured profile ─────────────────────────────────────────

    println!();
    println!("{}:", strings::get("drive.submit_header"));
    println!(
        "  {}:    {} {} {}",
        strings::get("drive.submit_drive"),
        id.vendor,
        id.product,
        id.revision
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

    // Package + present for manual submission: zip the captured profile and
    // print a ready-to-paste GitHub issue (title + body + issues/new URL). A
    // genuine I/O failure (zip or write) exits non-zero so scripts can detect it.

    print!("  {}  ", strings::get("drive.submit_packaging"));
    let _ = std::io::stdout().flush();
    let zip_data = match zip_files(&profile_dir, &written) {
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
    body.push_str(&format!("Manufacturer:    {}\n", id.vendor));
    body.push_str(&format!("Product:         {}\n", id.product));
    body.push_str(&format!("Revision:        {}\n", id.revision));
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

    body.push_str(&format!("---\n*Captured by `{CAPTURE_COMMAND} --share`*\n"));

    let title = format!("Drive profile: {} {}", id.vendor, id.product);

    present_for_submission(&profile_name, &zip_path, &title, &body);

    // The captured profile (and its zip) are kept on disk so the user can
    // attach/paste them when filing the issue. Do NOT remove the dir.
}

// Print everything needed to file the drive-profile issue by hand: title,
// pre-filled URL, full body, and the saved zip path. Always exits cleanly.
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

    // Build-injected, issues-only PAT (FREEMKV_GH_TOKEN at build time) so the
    // secret lives in the binary, not source (GitHub's scanner revokes any
    // committed token). No token compiled in => manual flow below.
    let token = option_env!("FREEMKV_GH_TOKEN").unwrap_or("").trim();
    // Auto-submit needs an INTERACTIVE terminal — a closed/piped stdin can't
    // give informed consent, and EOF must never read as "yes" (profile carries
    // the drive serial unless --mask); otherwise fall through to manual flow.
    if may_prompt_for_consent(
        token,
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
    ) {
        println!();
        // Prompt is localized; a crafted catalog could show "[j/N]" with a bare
        // Enter treated as YES, exfiltrating the profile. Fix: bare Enter never
        // posts, and the affirmative check uses the SAME locale token as shown.
        eprint!(
            "{}",
            strings::get_or(
                "drive.submit_prompt",
                "Submit this profile to help expand drive support? [y/N] ",
            )
        );
        // Flush the stream the PROMPT went to. This used to flush stdout after
        // an eprint!, which is the wrong stream.
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let n = std::io::stdin().read_line(&mut input).unwrap_or(0);
        let ans = input.trim();
        let affirmative = strings::get_or("drive.submit_affirmative", "y");
        // Consent must be EXPLICIT: only the locale's affirmative token posts;
        // a bare Enter or EOF (n==0) is never consent (see prompt comment above).
        if consent_granted(n, ans, &affirmative) {
            match submit_issue(token, title, body) {
                Some(url) => {
                    println!();
                    println!(
                        "{}",
                        strings::get_or("drive.submit_thanks", "Submitted — thank you!")
                    );
                    println!("  {url}");
                    return;
                }
                None => {
                    println!();
                    println!(
                        "{}",
                        strings::get_or(
                            "drive.submit_auto_failed",
                            "Automated submission failed; you can still file it by hand:",
                        )
                    );
                    // fall through to the manual instructions
                }
            }
        } else {
            println!(
                "{}",
                strings::get_or(
                    "drive.submit_declined",
                    "Not submitted. You can still file it by hand if you like:",
                )
            );
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

// Whether to offer the auto-submit prompt at all: both stdin and stderr must
// be a terminal, or the question could go unseen while blocking on a read.
// See docs/info.md — may_prompt_for_consent.
fn may_prompt_for_consent(token: &str, stdin_is_tty: bool, stderr_is_tty: bool) -> bool {
    !token.is_empty() && stdin_is_tty && stderr_is_tty
}

// Whether the user EXPLICITLY consented to submit the profile: EOF (n == 0),
// a bare Enter, and anything but the locale's affirmative token all fail
// closed. See docs/info.md — consent_granted.
fn consent_granted(n: usize, answer: &str, affirmative: &str) -> bool {
    n > 0 && !answer.is_empty() && answer.eq_ignore_ascii_case(affirmative)
}

// POST a drive-profile issue to `freemkv/bdemu` via the GitHub Issues API.
// Returns the `html_url` on success, `None` on any failure (caller falls
// back to the manual print path). Uses `curl` to avoid an HTTP stack dep.
fn submit_issue(token: &str, title: &str, body: &str) -> Option<String> {
    let payload = format!(
        r#"{{"title":"{}","body":"{}","labels":["drive-profile"]}}"#,
        json_escape(title),
        json_escape(body)
    );

    let output = std::process::Command::new(curl_program())
        .args(curl_submit_args(token, &payload))
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

/// The repository `--share` files drive-profile issues against.
const SUBMIT_REPO: &str = "freemkv/bdemu";

/// Connect timeout for the auto-submit POST, in seconds. A DNS black hole or a
/// dropped SYN must not park the CLI at the end of a capture the user has
/// already been shown.
const SUBMIT_CONNECT_TIMEOUT_SECS: u32 = 10;

// Whole-operation timeout for the auto-submit POST, in seconds. Generous
// (body carries a base64 zip) but FINITE, so a trickling peer can't hold
// the process open with no way out but Ctrl-C.
const SUBMIT_MAX_TIME_SECS: u32 = 120;

/// Cap on the response body. The reply is a GitHub issue JSON of a few KiB;
/// this bounds what a hostile or misbehaving endpoint can make us buffer while
/// scanning it for `html_url`.
const SUBMIT_MAX_FILESIZE_BYTES: u32 = 1024 * 1024;

// Which `curl` to run: named absolutely on Windows (CWE-427, a bare name
// there searches the app dir and CWD before System32); bare on Unix, where
// PATH doesn't. See docs/info.md — curl_program.
fn curl_program() -> String {
    // `SystemRoot` is set by the OS on every Windows session; if something has
    // unset it there is no trustworthy absolute path to build, so fall back to
    // the bare name rather than guess at `C:\Windows`.
    let root = if cfg!(target_os = "windows") {
        std::env::var("SystemRoot").ok()
    } else {
        None
    };
    curl_program_from(root.as_deref())
}

/// The path half of [`curl_program`], split out so it is testable off Windows —
/// the platform decision is the caller's, the string building is here.
fn curl_program_from(system_root: Option<&str>) -> String {
    match system_root {
        Some(root) => format!(r"{root}\System32\curl.exe"),
        None => "curl".to_string(),
    }
}

// The exact `curl` argv the auto-submit POST runs, split out of
// `submit_issue` so it's testable without a real GitHub request. `-f` is
// deliberately NOT passed. See docs/info.md — curl_submit_args.
fn curl_submit_args(token: &str, payload: &str) -> Vec<String> {
    [
        "-s",
        "-X",
        "POST",
        // Bound the connect, the whole operation, and the response body. A
        // silent hang here is worse than a failed submission: the manual
        // fall-back below always works.
        "--connect-timeout",
        &SUBMIT_CONNECT_TIMEOUT_SECS.to_string(),
        "--max-time",
        &SUBMIT_MAX_TIME_SECS.to_string(),
        "--max-filesize",
        &SUBMIT_MAX_FILESIZE_BYTES.to_string(),
        // The endpoint is https and fixed; refuse to be redirected off it.
        "--proto",
        "=https",
        &format!("https://api.github.com/repos/{SUBMIT_REPO}/issues"),
        "-H",
        &format!("Authorization: token {token}"),
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: freemkv-info",
        "-d",
        payload,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
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

// Archive exactly the named files from `dir` — a manifest, not a directory
// walk, since the archive can reach a public tracker. A missing name is
// skipped rather than failing the submission. See docs/info.md — zip_files.
fn zip_files(
    dir: &std::path::Path,
    names: &[String],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::io::Cursor;
    let buf = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut seen: Vec<&str> = Vec::new();
    for name in names {
        // Never our own output, and never the same entry twice (a duplicate
        // start_file produces an archive some extractors reject).
        if name == "profile.zip" || seen.contains(&name.as_str()) {
            continue;
        }
        let path = dir.join(name);
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        seen.push(name);
        zip.start_file(name, options)?;
        zip.write_all(&data)?;
    }

    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

// Write one capture file and RECORD its name in `written`: the manifest is
// not bookkeeping, it's what bounds the archive. See `zip_files`.
fn save_bin(dir: &std::path::Path, name: &str, data: &[u8], written: &mut Vec<String>) {
    let path = dir.join(name);
    if let Err(e) = std::fs::write(&path, data) {
        // `error.cannot_write` already exists and already carries exactly
        // this pair — a second, English-only phrasing of the same failure is
        // the drift this catalog exists to prevent.
        eprintln!(
            "{}",
            strings::fmt(
                "error.cannot_write",
                &[
                    ("path", path.display().to_string().as_str()),
                    ("error", e.to_string().as_str()),
                ],
            )
        );
        std::process::exit(1);
    }
    written.push(name.to_string());
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

// Reduce an untrusted firmware-derived string to a safe single path
// component (lowercase alnum/-/_ only; never `.`, `..`, or a separator).
// Falls back to `drive` if empty. See docs/info.md — sanitize_component.
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

// Escape a string for embedding inside a TOML basic (double-quoted) string:
// drive identity fields are raw firmware bytes and can contain `"`, `\`, or
// control chars that would break `key = "..."`. See docs/info.md.
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
    // Byte-index slices below are only sound on ASCII; a corrupted/non-ASCII
    // firmware-date field could panic on a mid-char split. Guard with
    // `is_ascii()` and pass through raw for anything unexpected.
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

// Decode a TOML basic string body, the inverse of `toml_escape`. Test-only:
// proves the encoder round-trips without a full TOML parser dependency.
// Panics on a malformed escape (the encoder must never emit one).
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
        CAPTURE_COMMAND, base64_decode, base64_encode, format_date, hex_dump, json_escape,
        sanitize_component, toml_basic_unescape, toml_escape,
    };

    #[test]
    fn capture_command_is_a_real_subcommand() {
        // Stamped into the TOML header and the `--share` GitHub issue body, so a
        // wrong command is published to a public tracker, not just one user. It
        // shipped as `freemkv drive-info`, which the dispatcher never accepted.
        let word = CAPTURE_COMMAND
            .strip_prefix("freemkv ")
            .unwrap_or_else(|| panic!("CAPTURE_COMMAND must invoke freemkv: {CAPTURE_COMMAND}"))
            .split_whitespace()
            .next()
            .expect("a command word");
        assert!(
            crate::cli_entry::SUBCOMMANDS.contains(&word),
            "the drive-profile capture command is published in shared artifacts but \
             `{word}` is not a subcommand the dispatcher accepts \
             ({:?})",
            crate::cli_entry::SUBCOMMANDS
        );
    }

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

    // A drive whose firmware answers INQUIRY with terminal escapes — junk a
    // wedged USB-SATA bridge produces routinely. See docs/info.md.
    fn hostile_drive_id() -> libfreemkv::DriveId {
        libfreemkv::DriveId {
            vendor_id: " HL-DT-ST\u{1b}[31m ".into(),
            product_id: "BD-RE\u{1b}]0;pwned\u{7}".into(),
            product_revision: "1.0\nmanufacturer = \"forged\"".into(),
            vendor_specific: "NC\u{202e}xyz".into(),
            firmware_date: "2021\u{0}0304".into(),
            serial_number: "SER\u{1b}[2J1234".into(),
            raw_inquiry: Vec::new(),
            raw_gc_010c: Vec::new(),
        }
    }

    /// The drive block `freemkv info disc://` prints is firmware-controlled
    /// text on a real terminal. `disc_info.rs` sanitises the identical class of
    /// field; this module printed every one of them verbatim.
    #[test]
    fn the_printed_drive_block_carries_no_firmware_escape_sequences() {
        let lines = super::drive_identity_lines(&hostile_drive_id(), "/dev/sg0", false);
        for line in &lines {
            for c in line.chars() {
                assert!(
                    !crate::strings::is_unsafe_display_char(c),
                    "a printed drive line still carries {c:?}: {line:?}"
                );
            }
        }
        let block = lines.join("\n");
        // The identifying text itself survives — this is display sanitisation,
        // not redaction. Expectations are literals, not re-derived.
        assert!(block.contains("HL-DT-ST[31m"), "{block}");
        assert!(block.contains("BD-RE]0;pwned"), "{block}");
        assert!(block.contains("1.0manufacturer = \"forged\""), "{block}");
        assert!(block.contains("SER[2J1234"), "{block}");
        // The device path is ours, and the block is exactly six lines.
        assert!(block.contains("/dev/sg0"), "{block}");
        assert_eq!(lines.len(), 6, "{lines:?}");
    }

    /// `--mask` must mask the SANITISED serial: masking first and sanitising
    /// never would leave the escape in an artifact meant to be publishable.
    #[test]
    fn a_masked_serial_is_derived_from_the_sanitised_one() {
        let lines = super::drive_identity_lines(&hostile_drive_id(), "/dev/sg0", true);
        let block = lines.join("\n");
        assert!(!block.contains("SER"), "serial not masked: {block}");
        assert!(
            !block.contains('\u{1b}'),
            "escape survived masking: {block:?}"
        );
    }

    /// The one unescaped firmware string in `drive.toml`. A comment ends at a
    /// newline, so a vendor id carrying one used to end the comment and hand
    /// the rest of the string to the TOML parser as a key.
    #[test]
    fn the_toml_header_comment_is_one_line_whatever_the_firmware_says() {
        let id = super::DriveIdentity::from_drive(&hostile_drive_id());
        let header = super::toml_header_comment(&id);
        assert!(header.starts_with("# "), "{header:?}");
        assert_eq!(
            header.trim_end_matches('\n').lines().count(),
            1,
            "the header comment must be a single line: {header:?}"
        );
        assert!(
            !header.contains("\nmanufacturer"),
            "a firmware newline forged a TOML key: {header:?}"
        );
        assert!(header.ends_with("\n\n"), "{header:?}");
    }

    // On Windows the program to run must be named absolutely, or a hostile
    // `curl.exe` on the app dir/CWD wins the search over System32's.
    // See docs/info.md — the_submit_curl_is_named_absolutely....
    #[test]
    fn the_submit_curl_is_named_absolutely_where_the_search_order_is_unsafe() {
        assert_eq!(
            super::curl_program_from(Some(r"C:\Windows")),
            r"C:\Windows\System32\curl.exe",
            "a bare name lets the app directory and the CWD win"
        );
        // No SystemRoot to build from (and every non-Windows host): the bare
        // name is the honest answer, not a guessed absolute path.
        assert_eq!(super::curl_program_from(None), "curl");
    }

    // The auto-submit POST is the LAST thing `--share` does; it shipped with
    // no bound on connect/total time/response size, so a stalled peer hung
    // the command after the work was already on disk. See docs/info.md.
    #[test]
    fn the_auto_submit_post_is_bounded_in_time_and_size() {
        let args = super::curl_submit_args("tok", "{}");
        let pair = |flag: &str| -> String {
            let i = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("`{flag}` missing from the curl argv: {args:?}"));
            args.get(i + 1)
                .unwrap_or_else(|| panic!("`{flag}` has no value: {args:?}"))
                .clone()
        };
        // Literals, not the constants: a mutation of either would otherwise
        // agree with itself.
        assert_eq!(pair("--connect-timeout"), "10");
        assert_eq!(pair("--max-time"), "120");
        assert_eq!(pair("--max-filesize"), "1048576");
        assert_eq!(pair("--proto"), "=https");
        // Still the same request it always was.
        assert!(args.contains(&"POST".to_string()), "{args:?}");
        assert!(
            args.contains(&"https://api.github.com/repos/freemkv/bdemu/issues".to_string()),
            "{args:?}"
        );
        assert!(
            args.contains(&"Authorization: token tok".to_string()),
            "{args:?}"
        );
        assert!(args.contains(&"{}".to_string()), "the payload: {args:?}");
        // Redirect-following is off (curl's default) and must stay off — the
        // request carries a bearer token.
        assert!(
            !args.iter().any(|a| a == "-L" || a == "--location"),
            "the POST carries a token; it must not follow redirects: {args:?}"
        );
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
        // Regression (HIGH): firmware strings were embedded unescaped into
        // `drive.toml`, so `"`, `\`, or newline produced an unparseable file.
        // Every value must escape to a form that decodes back to the original.
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

#[cfg(test)]
mod share_safety_tests {
    use super::{consent_granted, may_prompt_for_consent, zip_files};

    // Submitting requires EXPLICIT consent with the locale's accept token,
    // never a hard-coded ASCII "y" — a crafted `de.json` can render "[j/N]"
    // (default NO), so a bare Enter must not post. See docs/info.md.
    #[test]
    fn submitting_requires_an_explicit_locale_matched_yes() {
        // A bare Enter is NOT consent — it declines, whatever the prompt hinted.
        assert!(!consent_granted(1, "", "y"), "a bare Enter must not post");
        assert!(!consent_granted(1, "", "j"), "a bare Enter must not post");
        // EOF (n == 0) is never consent, even if the buffer somehow looks right.
        assert!(!consent_granted(0, "y", "y"), "EOF is never consent");
        // The affirmative token is the locale's, matched case-insensitively.
        assert!(consent_granted(1, "y", "y"));
        assert!(consent_granted(1, "Y", "y"));
        assert!(
            consent_granted(1, "j", "j"),
            "German 'j' posts under a de prompt"
        );
        assert!(consent_granted(1, "J", "j"));
        // A German 'j' must NOT post while the ASCII 'y' token is active, and an
        // English 'y' must not post under a German 'j' prompt — the token and
        // the prompt are the same locale, so a mismatch fails closed.
        assert!(!consent_granted(1, "j", "y"));
        assert!(!consent_granted(1, "y", "j"));
        // Anything else declines.
        assert!(!consent_granted(1, "n", "y"));
        assert!(!consent_granted(1, "yes please", "y"));
    }
    use std::io::Read as _;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // A scratch directory unique to this process and call — a fixed name is
    // shared state, and two `cargo test` processes deleting each other's
    // fixtures mid-assertion reads as a (false) share-safety failure.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("fmkv-{}-{}-{}", tag, std::process::id(), n))
    }

    fn entries(zip: &[u8]) -> Vec<String> {
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(zip.to_vec())).expect("valid zip");
        (0..a.len())
            .map(|i| a.by_index(i).unwrap().name().to_string())
            .collect()
    }

    // The archive is bounded by what this run WROTE, not what happens to be
    // in the directory (which could include a previous UNMASKED run's
    // capture, or unrelated local files). See docs/info.md.
    #[test]
    fn the_archive_carries_only_the_files_this_run_wrote() {
        let dir = scratch_dir("zip-manifest-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("inquiry.bin"), b"this run").unwrap();
        std::fs::write(dir.join("drive.toml"), b"[drive]\n").unwrap();
        // Left over from an earlier, UNMASKED run of a different drive.
        std::fs::write(dir.join("gc_0108.bin"), b"stale serial").unwrap();
        // Nothing to do with freemkv at all.
        std::fs::write(dir.join("tax-return.pdf"), b"private").unwrap();

        let got =
            entries(&zip_files(&dir, &names(&["inquiry.bin", "drive.toml"])).expect("zip built"));
        assert_eq!(got, vec!["inquiry.bin", "drive.toml"]);
        assert!(
            !got.iter().any(|n| n == "gc_0108.bin"),
            "a previous run's unmasked capture was published: {got:?}"
        );
        assert!(
            !got.iter().any(|n| n == "tax-return.pdf"),
            "an unrelated local file was published: {got:?}"
        );

        // The contents really are the named files, not just the names.
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(
            zip_files(&dir, &names(&["inquiry.bin"])).unwrap(),
        ))
        .unwrap();
        let mut body = String::new();
        a.by_name("inquiry.bin")
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "this run");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest entry that is not on disk, a duplicate, and our own output
    /// are all handled without failing the submission or corrupting the zip.
    #[test]
    fn a_missing_duplicate_or_self_referential_entry_does_not_break_the_archive() {
        let dir = scratch_dir("zip-edge-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inquiry.bin"), b"x").unwrap();
        std::fs::write(dir.join("profile.zip"), b"previous archive").unwrap();

        let got = entries(
            &zip_files(
                &dir,
                &names(&[
                    "inquiry.bin",
                    "inquiry.bin",
                    "never_written.bin",
                    "profile.zip",
                ]),
            )
            .expect("a missing entry must not fail the submission"),
        );
        assert_eq!(
            got,
            vec!["inquiry.bin"],
            "the archive must not nest itself or repeat an entry"
        );

        // An empty manifest is a valid, empty archive rather than an error.
        assert!(entries(&zip_files(&dir, &[]).expect("empty manifest is valid")).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // The consent prompt is only offered when the user can actually READ it:
    // testing stdin alone let `--share 2>/dev/null` block on a read with
    // nothing on screen, and bare Enter is the [Y] default. See docs/info.md.
    #[test]
    fn consent_is_only_offered_when_the_question_is_visible_and_answerable() {
        // Both channels interactive: ask.
        assert!(may_prompt_for_consent("tok", true, true));

        // stderr redirected — the question would be invisible.
        assert!(!may_prompt_for_consent("tok", true, false));
        // stdin redirected/piped — no informed answer is possible.
        assert!(!may_prompt_for_consent("tok", false, true));
        assert!(!may_prompt_for_consent("tok", false, false));

        // No compiled-in token: nothing to submit with, so never ask. (The
        // caller trims before passing, so the empty case covers whitespace.)
        assert!(!may_prompt_for_consent("", true, true));
    }
}
