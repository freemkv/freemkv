use super::{
    CAPTURE_COMMAND, DriveFlags, DriveParse, base64_decode, base64_encode, files_mode_2a_line,
    format_date, hex_dump, issue_url_from_reply, json_escape, parse_drive_flags,
    sanitize_component, toml_basic_unescape, toml_escape,
};

// GitHub pretty-prints its reply; the new issue's url has a space after the colon.
#[test]
fn the_issue_url_is_found_in_a_pretty_printed_reply() {
    let reply = "{\n  \"url\": \"https://api.github.com/repos/o/r/issues/7\",\n  \
            \"html_url\": \"https://github.com/o/r/issues/7\",\n  \"user\": {\n    \
            \"html_url\": \"https://github.com/someone\"\n  }\n}";
    assert_eq!(
        issue_url_from_reply(reply).as_deref(),
        Some("https://github.com/o/r/issues/7")
    );
    assert_eq!(
        issue_url_from_reply("{\"html_url\":\"https://github.com/o/r/issues/8\"}").as_deref(),
        Some("https://github.com/o/r/issues/8")
    );
    assert_eq!(
        issue_url_from_reply("{\"message\": \"Bad credentials\"}"),
        None
    );
}

// drive.toml lists only files that were written.
#[test]
fn mode_2a_is_listed_only_when_captured() {
    assert!(files_mode_2a_line(true).contains("mode_2a.bin"));
    assert!(!files_mode_2a_line(false).contains("mode_2a"));
}

// `--mask` decides whether the serial is published, so the parser must set it.
#[test]
fn the_mask_and_short_flags_set_their_fields() {
    for f in ["--mask", "-m"] {
        let DriveParse::Ok(flags) = parse_drive_flags(&[f.to_string()]) else {
            panic!("{f}")
        };
        assert!(flags.mask && !flags.share, "{f}");
    }
    let DriveParse::Ok(flags) = parse_drive_flags(&["-s".to_string(), "--quiet".to_string()])
    else {
        panic!()
    };
    assert!(flags.share && flags.quiet && !flags.mask);
    for f in ["-v", "-vv", "-vvv", "--verbose"] {
        let DriveParse::Ok(flags) = parse_drive_flags(&[f.to_string()]) else {
            panic!("{f}")
        };
        assert!(flags.verbose, "{f}");
    }
}

// The boundary: level 2 already widens the output, level 1 does not.
#[test]
fn log_level_two_is_verbose_and_one_is_not() {
    let at = |n: &str| match parse_drive_flags(&["--log-level".to_string(), n.to_string()]) {
        DriveParse::Ok(f) => f.verbose,
        _ => panic!(),
    };
    assert!(at("2"));
    assert!(!at("1"));
}

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

// A drive whose firmware answers INQUIRY with terminal escapes — junk a wedged USB-SATA
// bridge produces routinely.
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

// On Windows the program to run must be named absolutely, or a hostile `curl.exe` on the
// app dir/CWD wins the search over System32's.
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

// T14: the stdin curl config is exactly one `header = "…"` line carrying the token.
#[test]
fn the_auth_config_is_a_single_quoted_header_line() {
    assert_eq!(
        super::curl_auth_config("ghp_abc123"),
        "header = \"Authorization: token ghp_abc123\"\n"
    );
}

// L7: the whole body stays within GitHub's cap; a small zip is inlined, a big one is not.
#[test]
fn the_zip_is_inlined_only_when_the_body_fits() {
    let footer = "---\nfooter\n";
    let mut small = String::from("head\n");
    assert!(super::push_zip_section(&mut small, "zip", "QUJD", footer));
    assert!(small.contains("QUJD") && small.ends_with(footer), "{small}");

    let mut big = String::from("head\n");
    let b64 = "A".repeat(super::GITHUB_BODY_MAX_CHARS);
    assert!(!super::push_zip_section(&mut big, "zip", &b64, footer));
    assert!(big.chars().count() <= super::BODY_INLINE_BUDGET_CHARS);
    // Headroom for CRLF: the budget body still fits after every `\n` becomes `\r\n`.
    let mut edge = String::new();
    let b64 = "A".repeat(super::BODY_INLINE_BUDGET_CHARS - 2_000);
    if super::push_zip_section(&mut edge, "zip", &b64, footer) {
        let crlf = edge.chars().count() + edge.matches('\n').count();
        assert!(crlf <= super::GITHUB_BODY_MAX_CHARS, "{crlf}");
    }
    assert!(
        big.contains("profile.zip") && big.ends_with(footer),
        "{big}"
    );
}

// L7: a real BD's payload is hundreds of KiB — as one argv element it is E2BIG on Linux
// and over the Windows command-line cap. The payload goes by FILE, never argv.
#[test]
fn the_payload_is_passed_by_file_not_argv() {
    let args = super::curl_submit_args("/tmp/p/submit-payload.json");
    let i = args
        .iter()
        .position(|a| a == "--data-binary")
        .unwrap_or_else(|| panic!("no --data-binary: {args:?}"));
    assert_eq!(args[i + 1], "@/tmp/p/submit-payload.json");
    assert!(!args.iter().any(|a| a == "-d" || a == "--data"), "{args:?}");
}

// The auto-submit POST is the LAST thing `--share` does; it shipped with no bound on
// connect/total time/response size, so a stalled peer hung the command after the work was
// already on disk.
#[test]
fn the_auto_submit_post_is_bounded_in_time_and_size() {
    let args = super::curl_submit_args("{}");
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
    // The auth header is read from stdin config (`--config -`), never argv.
    assert_eq!(pair("--config"), "-");
    // Still the same request it always was.
    assert!(args.contains(&"POST".to_string()), "{args:?}");
    assert!(
        args.contains(&"https://api.github.com/repos/freemkv/bdemu/issues".to_string()),
        "{args:?}"
    );
    // The bearer token must NOT be in the argv (visible in `ps`): it goes to
    // curl via the stdin config file instead.
    assert!(
        !args.iter().any(|a| a.contains("Authorization")),
        "the Authorization header must not appear in the argv: {args:?}"
    );
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

// LG drives (e.g. BU40N) report a bogus "21" century in the CCYYMMDDHHMI
// field; the real date is 20YY. Rendering the raw century gives 2118.
#[test]
fn format_date_lg_bogus_century_renders_as_20yy() {
    assert_eq!(format_date("211810241934"), "2018-10-24");
}

fn drive_args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn drive_flags_accept_log_level_and_its_value() {
    let want = DriveFlags {
        share: true,
        verbose: true,
        ..Default::default()
    };
    assert_eq!(
        parse_drive_flags(&drive_args(&["--log-level", "3", "--share"])),
        DriveParse::Ok(want)
    );
    assert_eq!(
        parse_drive_flags(&drive_args(&["--log-level", "1"])),
        DriveParse::Ok(DriveFlags::default())
    );
}

#[test]
fn drive_flags_log_file_without_value_keeps_next_flag() {
    let want = DriveFlags {
        share: true,
        ..Default::default()
    };
    assert_eq!(
        parse_drive_flags(&drive_args(&["--log-file", "--share"])),
        DriveParse::Ok(want)
    );
    assert_eq!(
        parse_drive_flags(&drive_args(&["--log-file", "/tmp/x.log", "-q"])),
        DriveParse::Ok(DriveFlags {
            quiet: true,
            ..Default::default()
        })
    );
}

#[test]
fn drive_flags_reject_unknown_and_honour_help() {
    assert_eq!(
        parse_drive_flags(&drive_args(&["--bogus"])),
        DriveParse::Unknown("--bogus".into())
    );
    assert_eq!(parse_drive_flags(&drive_args(&["-h"])), DriveParse::Help);
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
    assert!(dump.starts_with("00 01 02"), "{dump}");
    let lines: Vec<&str> = dump.split('\n').collect();
    assert_eq!(lines.len(), 2, "{dump}");
    assert_eq!(
        lines[0].split(' ').count(),
        32,
        "wrap after 32 bytes: {dump}"
    );
    assert_eq!(
        lines[1], "  20",
        "continuation indent, then byte 32: {dump}"
    );
}
