// freemkv info disc:// — Show drive information and capture profiles
// MIT — freemkv project. CLI is dumb — all drive data from libfreemkv.

use crate::output::{Level::Normal, Output};
use crate::strings;
use std::io::{IsTerminal, Write};
use std::path::Path;

// The invocation printed into artifacts that leave this machine (TOML header, issue body).
const CAPTURE_COMMAND: &str = "freemkv info disc://";

// The drive identity, as it is safe to put in front of a human: fields are sanitised ONCE here
// so no later site can print a raw firmware string.
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

// The drive-identity block `freemkv info disc://` prints, as lines. Sanitises the raw `DriveId`
// itself.
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

// The `drive.toml` header comment, and the blank line after it. NOT `toml_escape`d (a comment
// needs no escaping), safe only because `DriveIdentity` already stripped control chars.
fn toml_header_comment(id: &DriveIdentity) -> String {
    format!(
        "# {} {} {} — {CAPTURE_COMMAND}\n\n",
        id.vendor, id.product, id.revision
    )
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct DriveFlags {
    pub share: bool,
    pub mask: bool,
    pub quiet: bool,
    pub verbose: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) enum DriveParse {
    Ok(DriveFlags),
    Help,
    Unknown(String),
}

fn next_value(args: &[String], i: usize) -> Option<&String> {
    args.get(i + 1)
        .filter(|v| !crate::cli_entry::is_flag_token(v))
}

pub(crate) fn parse_drive_flags(args: &[String]) -> DriveParse {
    let mut f = DriveFlags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--share" | "-s" => f.share = true,
            "--mask" | "-m" => f.mask = true,
            "--quiet" | "-q" => f.quiet = true,
            "--verbose" | "-v" => f.verbose = true,
            // Log-level/log-file tokens are handled by main::init_logging;
            // accept them here so they aren't rejected as unknown options.
            "-vv" | "-vvv" => f.verbose = true,
            "--log-level" => {
                if let Some(v) = next_value(args, i) {
                    if v.parse::<u8>().is_ok_and(|n| n >= 2) {
                        f.verbose = true;
                    }
                    i += 1;
                }
            }
            "--log-file" => {
                if next_value(args, i).is_some() {
                    i += 1;
                }
            }
            "--help" | "-h" => return DriveParse::Help,
            other => return DriveParse::Unknown(other.to_string()),
        }
        i += 1;
    }
    DriveParse::Ok(f)
}

// `--share --help`, shared by the drive and the image/folder routes.
pub(crate) fn print_share_help() {
    println!("{}", strings::get("drive.share_usage"));
    println!();
    println!("  --share    {}", strings::get("drive.share_desc"));
    println!("  --mask     {}", strings::get("drive.mask_desc"));
    println!("  --quiet    {}", strings::get("app.opt_quiet"));
    println!("  --verbose  {}", strings::get("app.opt_verbose"));
    println!();
    println!(
        "{}",
        strings::get_or(
            "drive.share_image_note",
            "On iso:// and dir:// sources --share captures the disc structure; \
             --mask and --verbose have no effect there (there is no drive).",
        )
    );
}

// GET_CONFIG feature 0x0108 (Logical Unit Serial Number): after its 4-byte feature header the
// data is the drive's serial, which `--mask` hides.
const FEATURE_SERIAL: u16 = 0x0108;
const FEATURE_HEADER_LEN: usize = 4;

// The Pioneer READ_BUFFER 0xF1 reply opens with the drive's 12-byte serial.
const RB_F1_SERIAL_LEN: usize = 12;

pub fn run(device: Option<&str>, args: &[String]) {
    let DriveFlags {
        share,
        mask,
        quiet,
        verbose,
    } = match parse_drive_flags(args) {
        DriveParse::Ok(f) => f,
        DriveParse::Help => return print_share_help(),
        DriveParse::Unknown(opt) => crate::disc_info::reject_unknown_option(&opt),
    };

    let mut session = match device {
        Some(p) => freemkv_engine::drive::open(Path::new(p)).unwrap_or_else(|e| {
            eprintln!(
                "{}",
                strings::fmt(
                    "error.open_failed",
                    &[("device", p), ("error", &e.to_string())]
                )
            );
            crate::cli_entry::exit(1);
        }),
        None => libfreemkv::find_drive().unwrap_or_else(|| {
            eprintln!("{}", strings::get("error.no_drive"));
            crate::cli_entry::exit(1);
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
            crate::cli_entry::exit(1);
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
        crate::cli_entry::exit(1);
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
        if feat.code == FEATURE_SERIAL && mask && feat_data.len() > FEATURE_HEADER_LEN {
            let masked = freemkv_engine::mask_bytes(&feat_data[FEATURE_HEADER_LEN..]);
            feat_data[FEATURE_HEADER_LEN..FEATURE_HEADER_LEN + masked.len()]
                .copy_from_slice(&masked);
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
        if mask && data.len() >= RB_F1_SERIAL_LEN {
            let masked = freemkv_engine::mask_bytes(&data[..RB_F1_SERIAL_LEN]);
            data[..RB_F1_SERIAL_LEN].copy_from_slice(&masked);
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
    toml.push_str(&files_mode_2a_line(capture.mode_2a.is_some()));
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
        crate::cli_entry::exit(1);
    }
    written.push("drive.toml".to_string());

    // Also fold in the disc's structure metadata when media is present (issue #45
    // repros from the playlists, not the drive). Best-effort — no disc/unreadable
    // tree just yields a drive-only profile, as before.
    let disc_summary =
        crate::disc_capture::fold_structure(&profile_dir, &mut written, &mut session)
            .unwrap_or_else(|e| {
                if !quiet {
                    eprintln!(
                        "{}",
                        strings::fmt_or(
                            "drive.structure_unreadable",
                            "Disc structure not captured: {err}",
                            &[("err", &crate::pipe::fmt_err(&e))],
                        )
                    );
                }
                None
            });
    if let (Some(ds), false) = (&disc_summary, quiet) {
        crate::disc_capture::report_skipped(ds);
        if ds.file_count == 0 {
            eprintln!("{}", crate::disc_capture::all_skipped_line(ds));
        }
    }
    // All files refused = no structure in this profile; don't claim "0 files".
    let disc_summary = disc_summary.filter(|ds| ds.file_count > 0);

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
            println!();
            eprintln!("{}", zip_failed_line(&*e));
            crate::cli_entry::exit(1);
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
        crate::cli_entry::exit(1);
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

    // Disc structure, when media was present and readable — the selection-bug
    // repro surface (issue #45). The raw files ride in the same zip below.
    if let Some(ref ds) = disc_summary {
        body.push_str("### Disc structure\n\n```\n");
        body.push_str(&format!(
            "Structure files: {} ({} bytes)\n",
            ds.file_count, ds.total_bytes
        ));
        body.push_str("```\n\n");
        body.push_str(
            "Disc metadata only (playlists / clip info / nav) — no audio/video \
             essence, no AACS keys.\n\n",
        );
    }

    let inlined = push_zip_section(
        &mut body,
        "Profile data (base64 zip)",
        &zip_b64,
        &format!("---\n*Captured by `{CAPTURE_COMMAND} --share`*\n"),
    );

    let title = format!("Drive profile: {} {}", id.vendor, id.product);

    present_for_submission(&profile_name, &zip_path, &title, &body, inlined);

    // The captured profile (and its zip) are kept on disk so the user can
    // attach/paste them when filing the issue. Do NOT remove the dir.
}

// The localized "could not build the zip" line. Both `--share` routes exit on it (no text-only
// fallback exists), so it must not reuse `drive.zip_failed`, which promises one.
pub(crate) fn zip_failed_line(e: &dyn std::fmt::Display) -> String {
    strings::fmt_or(
        "share.zip_failed",
        "Could not build the profile zip: {error}",
        &[("error", &e.to_string())],
    )
}

/// Finish an issue body: the base64 zip in a `<details>` block when the whole body fits GitHub's
/// limit, else a note to attach `profile.zip`. Returns whether the zip was inlined.
pub(crate) fn push_zip_section(
    body: &mut String,
    summary: &str,
    zip_b64: &str,
    footer: &str,
) -> bool {
    let mut block = format!("<details><summary>{summary}</summary>\n\n```\n");
    for chunk in zip_b64.as_bytes().chunks(76) {
        block.push_str(&String::from_utf8_lossy(chunk));
        block.push('\n');
    }
    block.push_str("```\n\n</details>\n\n");
    let total = body.chars().count() + block.chars().count() + footer.chars().count();
    let inlined = total <= BODY_INLINE_BUDGET_CHARS;
    if inlined {
        body.push_str(&block);
    } else {
        body.push_str(
            "The profile zip is too large to include in this issue; please attach \
             `profile.zip` from the saved profile folder.\n\n",
        );
    }
    body.push_str(footer);
    inlined
}

// Print everything needed to file the drive-profile issue by hand: title,
// pre-filled URL, full body, and the saved zip path. Always exits cleanly.
pub(crate) fn present_for_submission(
    profile_name: &str,
    zip_path: &Path,
    title: &str,
    body: &str,
    zip_inlined: bool,
) {
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
    if !zip_inlined {
        println!(
            "{}",
            strings::fmt_or(
                "share.attach_zip",
                "The profile zip is too large for the issue text — attach {path} to the issue.",
                &[("path", &zip_path.display().to_string())],
            )
        );
    }

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
            let payload_file = zip_path.with_file_name("submit-payload.json");
            match submit_issue(token, title, body, &payload_file) {
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
    println!("  https://github.com/{SUBMIT_REPO}/issues/new");
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

// Whether to offer the auto-submit prompt at all: both stdin and stderr must be a terminal, or
// the question could go unseen while blocking on a read.
fn may_prompt_for_consent(token: &str, stdin_is_tty: bool, stderr_is_tty: bool) -> bool {
    !token.is_empty() && stdin_is_tty && stderr_is_tty
}

// Whether the user EXPLICITLY consented to submit the profile: EOF (n == 0), a bare Enter, and
// anything but the locale's affirmative token all fail closed.
fn consent_granted(n: usize, answer: &str, affirmative: &str) -> bool {
    n > 0 && !answer.is_empty() && answer.eq_ignore_ascii_case(affirmative)
}

// The `[files]` entry for mode_2a.bin, which is written only when the drive answered MODE SENSE
// 2A; the section ends with it.
fn files_mode_2a_line(captured: bool) -> String {
    if captured {
        "mode_2a = \"mode_2a.bin\"\n\n".to_string()
    } else {
        "\n".to_string()
    }
}

// POST a drive-profile issue to `freemkv/bdemu` via the GitHub Issues API.
// Returns the `html_url` on success, `None` on any failure (caller falls
// back to the manual print path). Uses `curl` to avoid an HTTP stack dep.
fn submit_issue(token: &str, title: &str, body: &str, payload_file: &Path) -> Option<String> {
    let payload = format!(
        r#"{{"title":"{}","body":"{}","labels":["drive-profile"]}}"#,
        json_escape(title),
        json_escape(body)
    );
    // By file, not argv: a BD-sized payload overflows the OS argv/command-line limits.
    if std::fs::write(payload_file, payload).is_err() {
        // A part-written payload (disk full) holds the drive profile: don't leave it behind.
        let _ = std::fs::remove_file(payload_file);
        return None;
    }
    let response = run_submit_curl(token, &payload_file.to_string_lossy());
    let _ = std::fs::remove_file(payload_file);
    response
}

// Run the POST and pull the new issue's `html_url` out of the reply.
fn run_submit_curl(token: &str, payload_file: &str) -> Option<String> {
    let mut child = std::process::Command::new(curl_program())
        .args(curl_submit_args(payload_file))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // The Authorization header goes in via a config file on stdin, never argv (`ps`).
    {
        use std::io::Write;
        let mut stdin = child.stdin.take()?;
        let _ = stdin.write_all(curl_auth_config(token).as_bytes());
    }

    // Bounded read: never buffer more than the response cap, even if curl (or a
    // hostile endpoint) streams past `--max-filesize`. The reply is a few-KiB
    // issue JSON, so this only guards against a misbehaving peer.
    let mut stdout_bytes = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout
            .take(u64::from(SUBMIT_MAX_FILESIZE_BYTES))
            .read_to_end(&mut stdout_bytes);
    }
    let _ = child.wait();

    let response = String::from_utf8_lossy(&stdout_bytes);
    issue_url_from_reply(&response)
}

// Pull out "html_url": "…/issues/N" (skip the repo/user html_url fields). GitHub pretty-prints
// its reply, so whitespace around the colon is allowed.
fn issue_url_from_reply(response: &str) -> Option<String> {
    const KEY: &str = "\"html_url\"";
    for (idx, _) in response.match_indices(KEY) {
        let rest = response[idx + KEY.len()..].trim_start();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            let url = &rest[..end];
            if url.contains("/issues/") {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// GitHub's hard cap on an issue body, in characters.
pub(crate) const GITHUB_BODY_MAX_CHARS: usize = 65_536;

/// What `--share` lets a body grow to: headroom under the cap for a browser's CRLF newlines.
pub(crate) const BODY_INLINE_BUDGET_CHARS: usize = GITHUB_BODY_MAX_CHARS - 1_536;

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

// Which `curl` to run: named absolutely on Windows (CWE-427, a bare name there searches the app
// dir and CWD before System32); bare on Unix, where PATH doesn't.
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

// The stdin curl config carrying the Authorization header. Syntax is `header = "…"`; the token
// is opaque ASCII with no quotes, so no escaping.
fn curl_auth_config(token: &str) -> String {
    format!("header = \"Authorization: token {token}\"\n")
}

/// The exact `curl` argv the auto-submit POST runs, split out of `submit_issue` so it's
/// testable without a real GitHub request. `-f` is deliberately NOT passed. The bearer token is
/// deliberately NOT here: it is fed to curl as a config file on STDIN (see `submit_issue`) so
/// it never appears in this process's argv (visible in `ps`).
fn curl_submit_args(payload_file: &str) -> Vec<String> {
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
        // Read the Authorization header from a config file on stdin — keeps the
        // token out of argv. `-` is stdin; `submit_issue` writes the header there.
        "--config",
        "-",
        &format!("https://api.github.com/repos/{SUBMIT_REPO}/issues"),
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: freemkv-info",
        "--data-binary",
        &format!("@{payload_file}"),
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Minimal JSON string escaper for the issue payload (quotes, backslashes,
/// newlines, and control chars). The body carries base64 + backticks, so a
/// naive replace isn't enough.
pub(crate) fn json_escape(s: &str) -> String {
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

// Archive exactly the named files from `dir` — a manifest, not a directory walk, since the
// archive can reach a public tracker. A missing name is skipped rather than failing the
// submission.
pub(crate) fn zip_files(
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
pub(crate) fn save_bin(dir: &std::path::Path, name: &str, data: &[u8], written: &mut Vec<String>) {
    let path = dir.join(name);
    if let Err(e) = try_save_bin(dir, name, data) {
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
        crate::cli_entry::exit(1);
    }
    written.push(name.to_string());
}

// Write one capture file, creating its parent chain for a nested name
// (`BDMV/PLAYLIST/00800.mpls`). `name` must already be vetted: it is joined as-is.
pub(crate) fn try_save_bin(dir: &std::path::Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, data)
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

// Reduce an untrusted firmware-derived string to a safe single path component (lowercase
// alnum/-/_ only; never `.`, `..`, or a separator). Falls back to `drive` if empty.
pub(crate) fn sanitize_component(s: &str) -> String {
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

// Escape a string for embedding inside a TOML basic (double-quoted) string: drive identity
// fields are raw firmware bytes and can contain `"`, `\`, or control chars that would break
// `key = "..."`.
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

pub(crate) fn base64_encode(input: &[u8]) -> String {
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
#[path = "info_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "info_share_safety_tests.rs"]
mod share_safety_tests;
