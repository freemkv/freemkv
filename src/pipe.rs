//! Pipe — stream in, stream out.
//!
//! One pipeline for everything:
//!   1. disc→ISO: Disc::copy() (not a stream)
//!   2. Everything else: input → PES → output, one title at a time
//!
//! Batch (multiple titles) is just a for loop calling pipe() per title.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::pes::Stream as PesStream;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn install_signal_handler() {
    #[cfg(unix)]
    unsafe {
        // Register via sigaction, not signal(): on musl libc (the
        // cross-compiled deployment target) signal() is one-shot — the
        // disposition resets to SIG_DFL after the handler fires once, so the
        // second Ctrl-C would never re-enter handle_sigint and the
        // double-Ctrl-C _exit(130) guard would be dead. sigaction with
        // SA_RESTART (and no SA_RESETHAND) keeps the handler installed across
        // every delivery on both musl and glibc, and restarts slow syscalls.
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigint as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        // On failure, degrade gracefully: the handler simply isn't installed.
        let _ = libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }

    #[cfg(windows)]
    unsafe {
        extern "system" fn handler(_: u32) -> i32 {
            INTERRUPTED.store(true, Ordering::SeqCst);
            1
        }
        unsafe extern "system" {
            fn SetConsoleCtrlHandler(
                handler: unsafe extern "system" fn(u32) -> i32,
                add: i32,
            ) -> i32;
        }
        SetConsoleCtrlHandler(handler, 1);
    }
}

#[cfg(unix)]
extern "C" fn handle_sigint(_sig: libc::c_int) {
    if INTERRUPTED.load(Ordering::SeqCst) {
        unsafe { libc::_exit(130) };
    }
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Format an error for display using i18n strings.
///
/// libfreemkv errors render as `E<code>: <data>`. The no-key mux abort
/// (`E7022`, [`libfreemkv::Error::NoDiscKey`]) gets a dedicated message that
/// names the disc by hash; everything else falls through to the generic
/// wrapper.
pub fn fmt_err(e: &dyn std::fmt::Display) -> String {
    let s = e.to_string();
    fmt_err_str(&s)
}

/// Render a libfreemkv `E<code>[: <data>]` Display string (or any string) into
/// the user's language. The library emits errors as `E<code>` or
/// `E<code>: <data>` (see libfreemkv `error.rs` Display) with NO English; the
/// CLI owns all i18n. This parses the code, looks up `error.E<code>` in the
/// locale table, and renders it — for ANY code that has a locale entry — so no
/// raw `E####` ever reaches a user.
///
/// The data after the colon is passed as `{detail}` for the generic case, and
/// E7022 additionally exposes its disc hash as `{hash}` (its locale string
/// names the disc). A code with NO locale entry falls back to `error.generic`,
/// which still echoes the raw `E<code>: <data>` inside a localized wrapper —
/// the last-resort path, not the common one.
fn fmt_err_str(s: &str) -> String {
    if let Some((code_part, data)) = parse_error_code(s) {
        let key = format!("error.{code_part}");
        // `strings::get` returns the dotted path verbatim on a miss, so a
        // present locale entry is one whose lookup does NOT equal its own key.
        if strings::get(&key) != key {
            // WS2: the localized message is prefixed with its language-neutral
            // `E<code>` token — the code is SHOWN, not stripped. The `Error:`
            // level word is added once at the render site (`render_error` /
            // `main::fatal`), never here, so the fragment can also be embedded
            // as `{cause}`/`{detail}` inside a localized wrapper without
            // doubling the level prefix.
            let localized = if code_part == "E7022" {
                // E7022 names the disc by hash; keep its dedicated placeholder.
                strings::fmt(&key, &[("hash", data), ("detail", data)])
            } else if code_part == "E6000" {
                // E6000 (DiscRead) Display is `E6000: <sector> 0x..hex..` — the
                // status/sense hex tail is diagnostic noise that must not reach
                // the user. Pass ONLY the leading sector number as {detail}.
                let sector = data.split_whitespace().next().unwrap_or(data);
                strings::fmt(&key, &[("detail", sector)])
            } else {
                strings::fmt(&key, &[("detail", data)])
            };
            return format!("{code_part} {localized}");
        }
        // A code with NO locale entry still SHOWS its code via the generic
        // wrapper (`{code} {detail}`), so a missing string never swallows the
        // code. The contract test makes this unreachable for any real variant.
        return strings::fmt("error.generic", &[("code", code_part), ("detail", data)]);
    }
    // A non-code string (a CLI-side message): no code to show. The generic
    // wrapper is `{code} {detail}`; with an empty code that leaves a leading
    // space, so trim it — the render site adds the level word, and a stray
    // leading space would show as `Error:  msg`.
    strings::fmt("error.generic", &[("code", ""), ("detail", s)])
        .trim_start()
        .to_string()
}

/// Render an error for a user-facing terminal line, with the `Error:` level
/// word prefixed exactly once (WS2 §2.1). Inline render sites print this; the
/// `fatal()` block instead embeds the prefix-free `fmt_err` fragment as
/// `{cause}` inside `error.fatal_header` and adds the level word itself.
pub fn render_error(e: &dyn std::fmt::Display) -> String {
    let level = strings::get(crate::messaging::Level::Error.locale_key());
    format!("{}: {}", level, fmt_err(e))
}

/// Parse a libfreemkv Display string of the form `E<code>` or
/// `E<code>: <data>` into `("E<code>", "<data>")` (data empty when absent).
/// Returns `None` for any string that isn't an `E<digits>` code (so arbitrary
/// CLI error strings fall through to the generic wrapper unchanged).
fn parse_error_code(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('E')?;
    // The code is the leading run of digits after 'E'.
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits_end == 0 {
        return None; // "E" not followed by a digit — not a code.
    }
    let code = &s[..digits_end + 1]; // include the leading 'E'
    let after = &s[digits_end + 1..];
    // Data follows a ": " separator; absent for the bare `E<code>` form.
    let data = after.strip_prefix(':').map(|d| d.trim()).unwrap_or("");
    Some((code, data))
}

// ── CLI entry point ─────────────────────────────────────────────────────────

/// Flags parsed from the rip argument list.
#[derive(Default, Debug)]
struct ParsedFlags {
    verbose: bool,
    quiet: bool,
    raw: bool,
    multipass: bool,
    /// `--force`: overwrite into a non-empty `dir://` target.
    force: bool,
    keydb_path: Option<String>,
    key_url: Option<String>,
    key_auth: Option<String>,
    title_nums: Vec<usize>,
}

/// Where the CLI looks up AACS keys for a disc, assembled from the key flags.
///
/// libfreemkv does no lookup — the CLI resolves a [`libfreemkv::Key`] from these
/// sources and hands it to `Disc::decrypt_with`. When both `--keydb` and
/// `--key-url` are given, the keydb is consulted first (local-first), so an
/// offline hit never makes a key-service round-trip. Passing `--key-url` alone
/// bypasses the keydb entirely. See [`build_key_sources`] for the full
/// source-list policy.
#[derive(Default, Debug, Clone)]
pub struct KeyConfig {
    /// `--keydb PATH` — local `keydb.cfg` (else the standard location).
    keydb_path: Option<String>,
    /// `--key-url URL` — remote key-service base URL (enables the online source).
    key_url: Option<String>,
    /// `--key-auth TOKEN` — bearer token sent to the key service (optional).
    key_auth: Option<String>,
}

impl KeyConfig {
    /// The keydb path as an `Option<String>`, for the drive-handshake host-cert
    /// lookup (which always comes from a keydb, independent of the online source).
    fn keydb_path(&self) -> &Option<String> {
        &self.keydb_path
    }
}

/// Parse rip flags, returning a clear error string on any misuse:
/// - `-t`/`--title` with a missing, non-numeric, or `0` value (titles are
///   1-based; never silently fall through to "all titles").
/// - `--keydb` with a missing value (never silently use the default).
///
/// A value-flag will not consume a following positional URL token
/// (`scheme://...`) as its value — that means the value is missing.
fn parse_flags(args: &[String]) -> Result<ParsedFlags, String> {
    let mut f = ParsedFlags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // `--log-level N` sets the tracing level (main::init_logging); here
            // it widens prose detail at level >= 2. VAL-1: reject a non-numeric
            // or out-of-range value with a clean localized error rather than
            // silently ignoring it and leaving the user without a log file.
            "--log-level" => {
                match args.get(i + 1) {
                    Some(s) if !is_url_token(s) => {
                        i += 1;
                        match s.parse::<u8>() {
                            Ok(n) if n >= 1 => f.verbose = n >= 2,
                            _ => {
                                return Err(strings::fmt(
                                    "error.invalid_log_level",
                                    &[("value", s)],
                                ));
                            }
                        }
                    }
                    // No value or a URL follows: logging init already handles
                    // the bare --log-level case with its own plain-English
                    // diagnostic; nothing to do here.
                    _ => {}
                }
            }
            // `--log-file PATH` is consumed by logging init; swallow its value
            // here so the path isn't mistaken for a positional / unknown flag.
            "--log-file" => {
                if args.get(i + 1).is_some_and(|p| !is_url_token(p)) {
                    i += 1;
                }
            }
            "-q" | "--quiet" => f.quiet = true,
            "--raw" => f.raw = true,
            "--multipass" => f.multipass = true,
            "--force" => f.force = true,
            "-t" | "--title" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(v) if !is_url_token(v) => {
                        i += 1;
                        match v.parse::<usize>() {
                            Ok(n) if n >= 1 => f.title_nums.push(n),
                            _ => {
                                return Err(strings::fmt("error.invalid_title", &[("value", v)]));
                            }
                        }
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "-t 1")],
                        ));
                    }
                }
            }
            "--keydb" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(p) if !is_url_token(p) => {
                        i += 1;
                        f.keydb_path = Some(p.clone());
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "--keydb keydb.cfg")],
                        ));
                    }
                }
            }
            // `--key-url URL` enables the online key service. The URL must not be
            // a positional stream URL token (`scheme://...` other than http(s)) —
            // but a key-service URL IS `https://…`, which `is_url_token` matches
            // on "://". So accept it on its own merit: require an http(s) scheme
            // here, and reject a missing value (next token is a flag, or absent).
            // VAL-2: a non-http(s) URL (e.g. ftp://) gets its own clear error
            // rather than the confusing "requires a value" message, since the
            // user DID provide a value — it just has the wrong scheme.
            "--key-url" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(u) if is_keyserver_url(u) => {
                        i += 1;
                        f.key_url = Some(u.clone());
                    }
                    Some(u) if u.contains("://") && !is_keyserver_url(u) => {
                        // Has a scheme but it is NOT an http(s) key-service URL
                        // (e.g. `ftp://…`, or a stream scheme like `disc://`).
                        // The user DID supply a value — it just has the wrong
                        // scheme — so give the clear bad-scheme error instead of
                        // the misleading "requires a value". (`is_url_token` is
                        // exactly `contains("://")`, so the old guard was `A && !A`
                        // — dead code; key on the keyserver-scheme check instead.)
                        return Err(strings::fmt("error.key_url_bad_scheme", &[("value", u)]));
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[
                                ("flag", flag),
                                ("example", "--key-url https://keys.example/keys"),
                            ],
                        ));
                    }
                }
            }
            // `--key-auth TOKEN` — bearer token for the key service. A token is an
            // opaque string, not a URL; reject only a missing value (a following
            // stream-URL token means the token was omitted).
            "--key-auth" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(t) if !is_url_token(t) => {
                        i += 1;
                        f.key_auth = Some(t.clone());
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "--key-auth TOKEN")],
                        ));
                    }
                }
            }
            // An unrecognized dash-prefixed token is a typo (`--titel`,
            // `--qiet`), not something to silently ignore — the default would
            // be used and the rip would exit 0 having done the wrong thing.
            // Reject it. Bare `-` and non-dash positionals (URLs) are left for
            // the caller to interpret.
            other if other.starts_with('-') && other != "-" => {
                return Err(strings::fmt("error.unknown_flag", &[("flag", &args[i])]));
            }
            _ => {}
        }
        i += 1;
    }
    // Dedup repeated `-t` values: `-t 1 -t 1` is a no-op, not a double rip of
    // the same title (which would otherwise route into the multi-title branch
    // and produce two jobs that overwrite the same file). Sort so the rip order
    // is deterministic regardless of flag order.
    f.title_nums.sort_unstable();
    f.title_nums.dedup();
    Ok(f)
}

/// Returns true on success, false on error.
pub fn run(source: &str, dest: &str, args: &[String]) -> bool {
    install_signal_handler();

    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(msg) => {
            // Build a quiet-agnostic Output just to emit the error; flag parse
            // errors must surface even before we know verbose/quiet intent.
            Output::new(false, false).raw(Normal, &msg);
            return false;
        }
    };
    let ParsedFlags {
        verbose,
        quiet,
        raw,
        multipass,
        force,
        keydb_path,
        key_url,
        key_auth,
        title_nums,
    } = flags;

    let keys = KeyConfig {
        keydb_path,
        key_url,
        key_auth,
    };

    let out = Output::new(verbose, quiet);

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);

    let parsed_source = libfreemkv::parse_url(source);
    let parsed_dest = libfreemkv::parse_url(dest);

    // Fail loud and EARLY: validate the whole invocation (URL schemes, ISO-only
    // flags, source reachability, dest writability) BEFORE any drive open, scan,
    // or file creation. On any error this prints one clear message and returns
    // false (→ nonzero exit), so no partial output is ever produced. Each
    // individual check is small and unit-tested; this is the single entry point
    // that orders them.
    if let Err(msg) = preflight_validate(
        source,
        dest,
        &parsed_source,
        &parsed_dest,
        raw,
        multipass,
        force,
    ) {
        out.raw(Normal, &msg);
        return false;
    }

    // Disc → ISO or Disc → null: use Disc::copy() (not a stream)
    if matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. })
        && matches!(
            parsed_dest,
            libfreemkv::StreamUrl::Iso { .. } | libfreemkv::StreamUrl::Null
        )
    {
        return disc_to_iso(source, dest, &keys, raw, multipass, &out);
    }

    // Disc / ISO → dir://: decrypted file-tree extraction (Disc::extract_tree,
    // not a stream). Placed BEFORE the generic mux path: a `dir://` dest with a
    // disc-source input never flows through the PES/mux highway. Byte-stream
    // sources, `--raw`, and `--multipass` are already rejected by
    // `preflight_validate` above, so reaching here means the source is a disc.
    if matches!(parsed_dest, libfreemkv::StreamUrl::Dir { .. }) {
        return dir_to_extract(source, dest, &keys, &parsed_source, force, &out);
    }

    // Everything else: figure out titles, pipe each one
    // For disc with explicit -t, skip scan_titles (pipe_disc does its own scan)
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });

    // `--multipass` (and `--raw`) on a non-iso:// destination is rejected up
    // front by `preflight_validate` (iso://-only flags). The old silent
    // warn-and-ignore here is gone: reaching this point with `multipass` set
    // means the destination IS iso:// (handled by the disc_to_iso branch above)
    // or it's a non-disc source where multipass never applied. No action needed.
    // For a disc source we skip the upfront `scan_titles` (pipe_disc does its
    // own scan per title); we still need to honor MULTIPLE `-t` flags, so build
    // jobs straight from `title_nums` rather than collapsing to a single title.
    // Scan the ISO structure ONCE (keyless) and share it: titles here, unit keys
    // below (`resolve_iso_unit_keys`). A disc source scans per-title in `pipe_disc`.
    let iso_disc = if is_disc { None } else { scan_iso(source) };
    let titles = iso_disc.as_ref().map(|d| d.titles.clone());
    let is_dir_dest = dest.ends_with('/') || std::path::Path::new(parsed_dest.path_str()).is_dir();

    // Resolve the per-title indices we will rip. For a scanned source this comes
    // from its title list; for a disc source it comes straight from `title_nums`
    // (empty = single all-titles pass). Returns None after printing a directory-
    // creation error, in which case we abort with a non-zero exit.
    let jobs = match build_jobs(
        &titles,
        is_disc,
        &title_nums,
        is_dir_dest,
        dest,
        &parsed_dest,
        &out,
    ) {
        Some(j) => j,
        None => return false,
    };

    // Show summary for multi-title
    if let Some(ref t) = titles {
        if jobs.len() > 1 {
            out.raw(
                Normal,
                &strings::fmt(
                    "rip.titles_summary",
                    &[
                        ("total", &t.len().to_string()),
                        ("selected", &jobs.len().to_string()),
                    ],
                ),
            );
            out.blank(Normal);
        }
    }

    // Pipe each title
    let mut ok = true;

    // For an ISO source, resolve the AACS unit keys ONCE (keyless scan → local
    // keydb → decrypt_with) and hand them to each title's stream — libfreemkv
    // does no lookup. A disc source resolves per-title inside `pipe_disc`.
    let iso_unit_keys = match iso_disc {
        Some(disc) => resolve_iso_unit_keys(source, disc, &keys, &out),
        None => Vec::new(),
    };

    // Fresh-key-on-failure factory for the ISO mux: when an online key service
    // is configured, a unit no upfront key decrypts is re-tried by forwarding
    // that ciphertext to the service. `None` (no `--key-url`) keeps the prior
    // behaviour. Built once; cheap `Arc` clone per title below.
    let iso_key_fetch = if is_disc {
        None
    } else {
        build_iso_key_fetch(source, &keys)
    };

    // When the rip covers MORE THAN ONE title and the user did NOT name a
    // specific title (`-t N`), an incidental extra title that turns out to be
    // copy-protected-but-uncrackable (a 0.5 s menu stub, an FBI-warning loop,
    // any tiny CSS-locked nav title) must NOT abort the whole rip. We skip it
    // with a warning and keep muxing the rest. See `is_title_failure_fatal`.
    let multi_title = jobs.len() > 1;
    let explicit_selection = !title_nums.is_empty();

    for (title_idx, dest_url) in &jobs {
        // The MAIN FEATURE is title index 0 (the disc's primary title — first in
        // every title list throughout the codebase). A failure there is always a
        // hard error, even in an all-titles rip: the user wants the movie.
        let is_feature = title_idx.unwrap_or(0) == 0;
        // Print title info if we have it
        if let (Some(idx), Some(t)) = (title_idx, &titles) {
            if !title_in_range(*idx, t.len()) {
                eprintln!(
                    "{}",
                    strings::fmt(
                        "rip.warning_title_range",
                        &[
                            ("num", &(idx + 1).to_string()),
                            ("count", &t.len().to_string()),
                        ]
                    )
                );
                // An explicitly-requested out-of-range title is a hard failure,
                // not a warning-and-carry-on: without this the CLI would exit 0
                // despite ripping nothing for the requested title. (The disc
                // path enforces the same via pipe_disc returning Err.)
                ok = false;
                continue;
            }
            let title = &t[*idx];
            out.raw(
                Normal,
                &strings::fmt(
                    "rip.title_info",
                    &[
                        ("num", &(idx + 1).to_string()),
                        ("duration", &title.duration_display()),
                        ("size", &format!("{:.1}", title.size_gb())),
                    ],
                ),
            );
        }

        let result = if is_disc {
            // Disc source: use open_drive() directly — one session, no double init.
            pipe_disc(
                source,
                dest_url,
                title_idx.unwrap_or(0),
                &keys,
                raw,
                multipass,
                &out,
            )
        } else {
            // Non-disc (ISO): hand in the caller-resolved unit keys.
            let opts = libfreemkv::InputOptions {
                unit_keys: iso_unit_keys.clone(),
                title_index: *title_idx,
                raw,
                key_fetch: iso_key_fetch.clone(),
            };
            pipe(source, dest_url, &opts, &out)
        };

        if let Err(e) = result {
            if is_title_failure_fatal(&e, multi_title, explicit_selection, is_feature) {
                // The title the user actually wants (a `-t N` selection, or the
                // main feature) failed, or the failure is not a recoverable
                // per-title copy-protection skip — a genuine hard error. Print
                // it (E7023 / NoDiscKey / IO …) and fail the command.
                out.raw(Normal, &render_error(&e));
                ok = false;
            } else {
                // An incidental extra title in an all-titles rip is a stub:
                // either copy-protected-but-uncrackable (E7023) or empty / no
                // muxable frames (E6008, an empty nav/menu PGC that emits no
                // frames). `is_title_failure_fatal` classified it non-fatal, so
                // skip it with a clear, non-error notice and keep muxing the
                // rest; the command can still exit 0 if the feature / requested
                // titles succeed. NO FALSE ERRORS.
                let num = title_idx.map(|i| i + 1).unwrap_or(0);
                let key = match parse_error_code(&e) {
                    Some(("E6008", _)) => "rip.title_skipped_empty",
                    _ => "rip.title_skipped",
                };
                out.raw(Normal, &strings::fmt(key, &[("num", &num.to_string())]));
            }
        }
        out.blank(Normal);
    }

    ok
}

// ── Pre-flight invocation validation (fail loud and EARLY) ──────────────────

/// Whether a parsed destination URL targets an `iso://` image. `--raw` and
/// `--multipass` only apply to a raw disc-image output (they write/recover a
/// sector image with a mapfile); every other destination is a decode+mux and
/// must reject those flags. Centralized so the flag gate and any future caller
/// agree on the predicate.
fn dest_is_iso(parsed_dest: &libfreemkv::StreamUrl) -> bool {
    matches!(parsed_dest, libfreemkv::StreamUrl::Iso { .. })
}

/// Whether a destination is a scheme-only sink with no filesystem path —
/// `null://` (discard) or `stdio://` (stdout). Such a sink consumes every
/// selected title through the SAME URL: it can't be given per-title file names,
/// so the multi-title job builder must not route it through `dir_jobs` (which
/// would synthesize an invalid `null://stem_t1.null` path).
fn is_scheme_only_sink(parsed_dest: &libfreemkv::StreamUrl) -> bool {
    matches!(
        parsed_dest,
        libfreemkv::StreamUrl::Null | libfreemkv::StreamUrl::Stdio
    )
}

/// Validate the whole rip invocation BEFORE any drive open, scan, or file
/// creation. Returns `Err(message)` — a single, already-localized, ready-to-
/// print string — on the first problem, so the caller prints it and exits
/// non-zero with no partial output. `Ok(())` means every checked precondition
/// holds and the rip may proceed.
///
/// Checks, in order (cheapest / most-fundamental first):
/// 1. Source and destination both carry a URL scheme (`scheme://…`).
/// 2. `--raw` / `--multipass` are used only with an `iso://` destination.
/// 3. Source is reachable: a `disc://` device path that is given must exist; an
///    `iso://` input must exist, be a file (not a dir), and be non-empty.
/// 4. Destination is writable: for a single-file `mkv://`/`m2ts://`/`iso://`
///    output the parent directory must exist and be writable, and the path must
///    not already be a directory.
///
/// Deep validation (a real UDF/ISO filesystem probe, a live drive handshake) is
/// left to the scan step, which surfaces its own typed errors; this is the
/// cheap, side-effect-free gate that catches the common mistakes instantly.
fn preflight_validate(
    source: &str,
    dest: &str,
    parsed_source: &libfreemkv::StreamUrl,
    parsed_dest: &libfreemkv::StreamUrl,
    raw: bool,
    multipass: bool,
    force: bool,
) -> Result<(), String> {
    // 1a. Destination must have a recognized scheme. A schemeless dest
    // (`out.mkv`, `/path/out.mkv`) parses as Unknown — guide the user to add a
    // scheme rather than later failing with a cryptic StreamUrlInvalid or
    // writing `name_t1.unknown`.
    if matches!(parsed_dest, libfreemkv::StreamUrl::Unknown { .. }) {
        return Err(strings::fmt("error.dest_needs_scheme", &[("dest", dest)]));
    }
    // 1b. Source must have a recognized scheme too. A bare path as source would
    // otherwise fall through to a no-titles / cryptic error far downstream.
    if matches!(parsed_source, libfreemkv::StreamUrl::Unknown { .. }) {
        return Err(strings::fmt(
            "error.source_needs_scheme",
            &[("source", source)],
        ));
    }

    // 2. `--raw` and `--multipass` are iso://-output-only (deliberate design).
    // A non-ISO destination + either flag is a hard, early error with guidance —
    // never a silent ignore. Check raw first, then multipass, so the message
    // names the actual offending flag.
    if !dest_is_iso(parsed_dest) {
        if raw {
            return Err(strings::fmt("error.raw_iso_only", &[("dest", dest)]));
        }
        if multipass {
            return Err(strings::fmt("error.multipass_iso_only", &[("dest", dest)]));
        }
    }

    // 2b. `dir://` (decrypted file-tree extraction) gates. A `dir://` output
    // needs a filesystem source (disc:// or iso://) — a byte-stream source
    // (mkv://, m2ts://, network://, stdio://) has no UDF tree, so reject it up
    // front. (`--raw` / `--multipass` are already rejected by step 2, since
    // `dir://` is not `iso://`.) Writability/non-empty are checked in step 4.
    if matches!(parsed_dest, libfreemkv::StreamUrl::Dir { .. }) && !parsed_source.is_disc_source() {
        return Err(strings::fmt(
            "error.dir_source_unsupported",
            &[("source", source)],
        ));
    }

    // 3. Source reachability.
    match parsed_source {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => {
            // An explicitly named device must exist. (Auto-detect — device None —
            // is left to `find_drive`, which has its own "no drive" message.)
            if !p.exists() {
                return Err(strings::fmt(
                    "error.device_not_found",
                    &[("path", &p.display().to_string())],
                ));
            }
        }
        libfreemkv::StreamUrl::Iso { path } => {
            validate_iso_input(path)?;
        }
        _ => {}
    }

    // 4. Destination writability for a single-file output. Directory dests and
    // scheme-only sinks (null://, stdio://, network://) are not pre-checked here:
    // a directory dest is created on demand by `dir_jobs` (which reports its own
    // error), and the sinks have no filesystem path to validate.
    match parsed_dest {
        libfreemkv::StreamUrl::Mkv { path }
        | libfreemkv::StreamUrl::M2ts { path }
        | libfreemkv::StreamUrl::Iso { path } => {
            // A trailing-slash dest (one-file-per-title directory) is validated by
            // dir_jobs, not here.
            if !dest.ends_with('/') {
                validate_file_dest(path)?;
            }
        }
        // `dir://` target: must be creatable + writable, and (unless --force)
        // empty. The producer re-checks these, but surfacing them here gives a
        // clean localized message with zero side effects.
        libfreemkv::StreamUrl::Dir { path } => {
            validate_dir_dest(path, dest, force)?;
        }
        // `demux://` writes per-track ES files into a directory (created on
        // demand). Same creatable/writable/non-empty gate as `dir://`.
        libfreemkv::StreamUrl::Demux { dir } => {
            validate_dir_dest(dir, dest, force)?;
        }
        _ => {}
    }

    Ok(())
}

/// Validate a `dir://` destination: the path must be creatable and writable
/// (it is created if absent), must not be an existing regular file, and —
/// unless `force` — must be empty.
fn validate_dir_dest(path: &std::path::Path, dest: &str, force: bool) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(strings::fmt("error.dir_dest_invalid", &[("dest", dest)]));
    }
    if path.is_file() {
        return Err(strings::fmt(
            "error.dir_dest_is_file",
            &[("path", &path.display().to_string())],
        ));
    }
    // Side-effect-free preflight: do NOT create the directory here. The write
    // path (`dir_jobs`) does `create_dir_all` and fails fast with a clear message
    // if it can't, so creating it here only risks leaving a stray empty dir when a
    // later step fails. A missing dir reads as empty below (created at write time).
    if !force {
        let non_empty = std::fs::read_dir(path)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if non_empty {
            return Err(strings::fmt(
                "error.dir_dest_not_empty",
                &[("path", &path.display().to_string())],
            ));
        }
    }
    Ok(())
}

/// Validate an `iso://` input path: must exist, be a regular file (not a
/// directory), and be non-empty. A deeper "is it a real disc image?" probe is
/// the scan's job; this catches the instant mistakes (typo'd path, a directory,
/// a 0-byte stub) before any scan work.
fn validate_iso_input(path: &std::path::Path) -> Result<(), String> {
    let md = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(strings::fmt(
                "error.iso_not_found",
                &[("path", &path.display().to_string())],
            ));
        }
        Err(e) => {
            return Err(strings::fmt(
                "error.iso_not_readable",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &e.to_string()),
                ],
            ));
        }
    };
    if md.is_dir() {
        return Err(strings::fmt(
            "error.iso_is_dir",
            &[("path", &path.display().to_string())],
        ));
    }
    if md.len() == 0 {
        return Err(strings::fmt(
            "error.iso_empty",
            &[("path", &path.display().to_string())],
        ));
    }
    // Readability: opening for read is cheap and catches permission errors that
    // `metadata` (which only needs directory-traverse) would miss.
    if let Err(e) = std::fs::File::open(path) {
        return Err(strings::fmt(
            "error.iso_not_readable",
            &[
                ("path", &path.display().to_string()),
                ("error", &e.to_string()),
            ],
        ));
    }
    Ok(())
}

/// Validate a single-file destination path: the parent directory must exist,
/// the path must not already be a directory, and the location must be writable.
/// Catches "parent dir doesn't exist" and "no write permission" up front instead
/// of after a scan + mux has already run.
fn validate_file_dest(path: &std::path::Path) -> Result<(), String> {
    // An existing directory at the file path can't receive a single-file write.
    if path.is_dir() {
        return Err(strings::fmt(
            "error.dest_is_dir_as_file",
            &[("path", &path.display().to_string())],
        ));
    }
    // The parent directory must exist. `parent()` is None for a bare filename
    // (e.g. `out.mkv`) → parent is the current dir, which exists; treat empty
    // parent as "." so a cwd-relative filename is allowed.
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    if !parent.exists() {
        return Err(strings::fmt(
            "error.dest_parent_missing",
            &[("path", &parent.display().to_string())],
        ));
    }
    // Writability probe: try to create (then remove) the target. This is the
    // honest test — directory write/exec permission, a read-only filesystem, an
    // existing read-only file all surface here. We only probe when the target
    // does not already exist (so we never truncate a real prior output during a
    // dry validation); if it exists, we check it's writable via its metadata.
    if path.exists() {
        match std::fs::OpenOptions::new().append(true).open(path) {
            Ok(_) => {}
            Err(e) => {
                return Err(strings::fmt(
                    "error.dest_not_writable",
                    &[
                        ("path", &path.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                ));
            }
        }
    } else {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => {
                // Remove the just-created probe file so the real mux creates it
                // fresh (with its size hint / fallocate). Best-effort cleanup.
                let _ = std::fs::remove_file(path);
            }
            Err(e) => {
                return Err(strings::fmt(
                    "error.dest_not_writable",
                    &[
                        ("path", &path.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                ));
            }
        }
    }
    Ok(())
}

/// Build the `(title_index, dest_url)` job list.
///
/// - Scanned source (ISO, etc.) with a title list: select the requested titles
///   (or all, when none given); one file when a single title goes to a file,
///   else one file per title in a directory.
/// - Disc source: there is no upfront title list, so build straight from
///   `title_nums`. Multiple `-t` flags each get their own job (writing to a
///   directory when more than one is selected) instead of silently dropping all
///   but the first. Empty `title_nums` is the single all-titles pass.
///
/// Returns `None` (after printing the error) if a needed output directory can't
/// be created, so the caller can exit non-zero.
fn build_jobs(
    titles: &Option<Vec<libfreemkv::DiscTitle>>,
    is_disc: bool,
    title_nums: &[usize],
    is_dir_dest: bool,
    dest: &str,
    parsed_dest: &libfreemkv::StreamUrl,
    out: &Output,
) -> Option<Vec<(Option<usize>, String)>> {
    // Lay out one file per selected title under a directory destination.
    // `disc_name` seeds the filename stem; falls back to "disc".
    let dir_jobs = |indices: &[usize], disc_name: &str| -> Option<Vec<(Option<usize>, String)>> {
        let ext = parsed_dest.scheme();
        let dest_dir = std::path::Path::new(parsed_dest.path_str());
        // Fail fast with one clear message if the output directory can't be
        // created (permissions, a file at that path, NFS stale handle).
        // Swallowing it here makes every per-title `output()` fail later with a
        // cryptic StreamUrlInvalid/IO error.
        if let Err(e) = std::fs::create_dir_all(dest_dir) {
            out.raw(
                Normal,
                &strings::fmt(
                    "error.cannot_create_dir",
                    &[
                        ("path", &dest_dir.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                ),
            );
            return None;
        }
        Some(
            indices
                .iter()
                .map(|&idx| {
                    let filename = format!("{}_t{}.{}", disc_name, idx + 1, ext);
                    let url = format!("{}://{}", ext, dest_dir.join(filename).display());
                    (Some(idx), url)
                })
                .collect(),
        )
    };

    // A scheme-only sink (null://, stdio://) has NO filesystem path, so it can
    // never receive per-title file naming. Multiple selected titles all route to
    // the SAME sink URL (each title decoded then discarded / streamed in turn).
    // Without this, the multi-title branches below call `dir_jobs`, which derives
    // an invalid `null://disc_t1.null` (a path on a scheme that must be empty) —
    // `parse_url` then rejects it (Unknown) and `output()` errors, so `null://`
    // wrongly failed on any multi-title source. `sink_jobs` maps each selected
    // index to the bare sink URL instead.
    let sink_jobs = |indices: &[usize]| -> Option<Vec<(Option<usize>, String)>> {
        Some(
            indices
                .iter()
                .map(|&idx| (Some(idx), dest.to_string()))
                .collect(),
        )
    };

    // `demux://` is a directory-target sink that fans each title's tracks out to
    // ES files INSIDE the directory (the sink does its own per-track naming).
    // A single title writes straight into `demux://<dir>/`; multiple titles each
    // get their own `demux://<dir>/t<NN>/` subdirectory so their files don't
    // collide. (Unlike `dir_jobs`, we never append a `.demux` filename — the
    // path stays a directory.)
    let demux_jobs = |indices: &[usize], base_dir: &str| -> Vec<(Option<usize>, String)> {
        if indices.len() == 1 {
            return vec![(Some(indices[0]), dest.to_string())];
        }
        let trimmed = base_dir.trim_end_matches('/');
        indices
            .iter()
            // Re-prefix the `demux://` scheme: `base_dir` came from
            // `path_str()` (scheme stripped), so a bare `out/t02/` would be
            // rejected by `parse_url` as Unknown and `output()` would error.
            .map(|&idx| (Some(idx), format!("demux://{trimmed}/t{:02}/", idx + 1)))
            .collect()
    };
    let demux_dir = parsed_dest.path_str().to_string();

    match titles {
        Some(t) if !t.is_empty() => {
            // Scanned source — select which titles.
            let indices: Vec<usize> = if title_nums.is_empty() {
                // ALL-TITLES rip (no `-t`): one job per scanned title.
                (0..t.len()).collect()
            } else {
                title_nums.iter().map(|n| n.saturating_sub(1)).collect()
            };
            if is_scheme_only_sink(parsed_dest) {
                // null:// / stdio:// — every title to the single sink, no naming.
                sink_jobs(&indices)
            } else if matches!(parsed_dest, libfreemkv::StreamUrl::Demux { .. }) {
                // demux:// — directory sink with its own per-track naming.
                Some(demux_jobs(&indices, &demux_dir))
            } else if indices.len() == 1 && !is_dir_dest {
                Some(vec![(Some(indices[0]), dest.to_string())])
            } else {
                let disc_name = t
                    .first()
                    .and_then(|ti| {
                        if ti.playlist.is_empty() {
                            None
                        } else {
                            Some(sanitize_name(&ti.playlist))
                        }
                    })
                    .unwrap_or_else(|| "disc".to_string());
                dir_jobs(&indices, &disc_name)
            }
        }
        _ if is_disc && title_nums.len() > 1 => {
            // Disc source, multiple titles requested. pipe_disc scans per title;
            // one job per requested title.
            let indices: Vec<usize> = title_nums.iter().map(|n| n.saturating_sub(1)).collect();
            if is_scheme_only_sink(parsed_dest) {
                // null:// / stdio:// — every requested title to the single sink.
                return sink_jobs(&indices);
            }
            if matches!(parsed_dest, libfreemkv::StreamUrl::Demux { .. }) {
                // demux:// — directory sink with its own per-track naming.
                return Some(demux_jobs(&indices, &demux_dir));
            }
            // A single-file dest can't hold multiple titles: `dir_jobs` would
            // `create_dir_all` it, silently turning `movie.mkv` into a directory.
            // Mirror the scanned-source guard above and reject up front. (The
            // scanned branch falls through to per-title-in-a-dir only when the
            // dest IS a directory; the disc branch must do the same.)
            if !is_dir_dest {
                out.raw(
                    Normal,
                    &strings::fmt("error.multi_title_needs_dir", &[("dest", dest)]),
                );
                return None;
            }
            dir_jobs(&indices, "disc")
        }
        _ => {
            // No title list, single pass (disc all-titles, single -t, or a
            // streaming source). `-t 0` was rejected during flag parsing, but
            // saturating_sub guards a stray 0 from underflowing to usize::MAX.
            let idx = title_nums.first().map(|n| n.saturating_sub(1));
            Some(vec![(idx, dest.to_string())])
        }
    }
}

// ── The pipeline engine ─────────────────────────────────────────────────────

/// Disc source: one open, one scan, one stream. No double init.
/// ScanOptions for a keyless structure scan — libfreemkv captures structure +
/// AACS inputs but resolves no key. The CLI resolves the key afterward from the
/// local keydb (see [`apply_local_key`]).
fn keyless_scan_opts() -> libfreemkv::ScanOptions {
    libfreemkv::ScanOptions::default()
}

/// ScanOptions for a **live-drive** scan: keyless, plus the AACS host
/// credentials for the authenticated handshake (sourced from the local keydb).
/// A locked drive needs the cert to read its Volume ID; an unlocked / LibreDrive
/// drive takes the OEM path and ignores them. ISO scans use [`keyless_scan_opts`].
pub(crate) fn drive_scan_opts(keydb_path: &Option<String>) -> libfreemkv::ScanOptions {
    let path = resolved_keydb_path(keydb_path);
    let host_certs = freemkv_keysources::KeydbSource::new(path).host_certs();
    let credentials =
        (!host_certs.is_empty()).then_some(libfreemkv::DriveCredentials { host_certs });
    libfreemkv::ScanOptions {
        credentials,
        ..Default::default()
    }
}

/// Resolve a **live drive's** AACS unit keys in place for `disc-info -v`: sample
/// ciphertext from the largest title and run the local-keydb key source against
/// it (no online source — `disc-info` never phones a key service). Populates
/// `disc.aacs.unit_keys` / `vuk` so the verbose crypto block can show a REAL
/// resolution instead of the keyless 0. No-op for an unencrypted / non-AACS disc
/// (`inputs()` returns `None`). The drive must still be open and have been
/// scanned with [`drive_scan_opts`] so the handshake captured the VID + inf.
pub(crate) fn resolve_info_keys(
    drive: &mut libfreemkv::Drive,
    disc: &mut libfreemkv::Disc,
    keydb_path: &Option<String>,
    out: &Output,
) {
    let samples = disc
        .titles
        .iter()
        .max_by_key(|t| t.size_bytes)
        .cloned()
        .map(|t| libfreemkv::read_encrypted_units(drive, &t, SAMPLE_UNITS))
        .unwrap_or_default();
    let keys = KeyConfig {
        keydb_path: keydb_path.clone(),
        key_url: None,
        key_auth: None,
    };
    apply_keys(disc, &keys, samples, out);
}

/// Scan an `iso://` source's structure ONCE (keyless). The resulting `Disc` is
/// shared by title enumeration and unit-key resolution so the ISO is not
/// re-parsed per step. `None` for a non-iso source or an unreadable image.
fn scan_iso(source: &str) -> Option<libfreemkv::Disc> {
    let path = match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Iso { path } => path,
        _ => return None,
    };
    let mut reader = libfreemkv::FileSectorSource::open(&path).ok()?;
    let capacity =
        <libfreemkv::FileSectorSource as libfreemkv::SectorSource>::capacity_sectors(&reader);
    libfreemkv::Disc::scan_image(&mut reader, capacity, &keyless_scan_opts()).ok()
}

/// Resolve an ISO's AACS unit keys from an already-scanned `Disc`: sample its
/// largest title, then local keydb, then decrypt_with. Empty for an unencrypted
/// ISO or when no key resolves.
fn resolve_iso_unit_keys(
    source: &str,
    mut disc: libfreemkv::Disc,
    keys: &KeyConfig,
    out: &Output,
) -> Vec<(u32, [u8; 16])> {
    let path = match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Iso { path } => path,
        _ => return Vec::new(),
    };
    // The ISO was already scanned once by the caller (`scan_iso`); we only open a
    // cheap reader here to sample ciphertext — no second structure scan.
    let Ok(mut reader) = libfreemkv::FileSectorSource::open(&path) else {
        return Vec::new();
    };
    // Sample encrypted units from the largest title so key resolution can
    // validate a keydb key against real ciphertext (and reject a wrong one).
    let samples = disc
        .titles
        .iter()
        .max_by_key(|t| t.size_bytes)
        .cloned()
        .map(|t| libfreemkv::read_encrypted_units(&mut reader, &t, SAMPLE_UNITS))
        .unwrap_or_default();
    apply_keys(&mut disc, keys, samples, out);
    match disc.decrypt_keys() {
        libfreemkv::DecryptKeys::Aacs { unit_keys, .. } => unit_keys,
        _ => Vec::new(),
    }
}

/// Build the fresh-key-on-failure closure for an ISO mux, or `None`.
///
/// When an online key service is configured (`--key-url`), this returns a shared
/// [`libfreemkv::sector::KeyFetch`] (built by [`libfreemkv::keysource::key_fetch`])
/// that the iso:// mux installs into the decrypt decorator. If a unit no held key
/// decrypts, the decorator hands that ciphertext to the closure, which forwards it
/// (as content samples) to the key service via [`freemkv_keysources::OnlineSource`]
/// and returns any unit keys the service derives — mirroring the DVD model (held
/// key first, ask the key source for the failing data). `None` when no key URL is
/// set, the URL is SSRF-rejected, or the source isn't an AACS ISO. The library
/// still makes no network call — this closure is the application's seam to the
/// key service. The fetch logic lives in the lib; the CLI supplies only the disc
/// inputs and the source builder.
fn build_iso_key_fetch(source: &str, keys: &KeyConfig) -> Option<libfreemkv::sector::KeyFetch> {
    let url = keys.key_url.clone()?;
    // Reuse the SSRF guard the upfront source list uses; a rejected URL means no
    // fetch (rather than POSTing key material to an internal/metadata host).
    if freemkv_keysources::validate_keyserver_url(&url).is_err() {
        return None;
    }
    let auth = keys.key_auth.clone().unwrap_or_default();
    let path = match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Iso { path } => path,
        _ => return None,
    };
    // Capture the disc's inf + MKB ONCE; a non-AACS ISO yields an error → None.
    let (inf, mkb, version) =
        libfreemkv::Disc::read_aacs_inputs(std::path::Path::new(&path)).ok()?;
    if inf.is_empty() {
        return None;
    }
    // Disc inputs the lib's `key_fetch` reuses per call (it swaps in the failing
    // `samples`). An ISO has no live-drive VID (all-zero) — VID-optional. The
    // version drives the Unit_Key_RO.inf stride for a VUK-from-server reply.
    let inputs = libfreemkv::DiscInputs {
        disc_hash: String::new(),
        volume_id: [0u8; 16],
        version,
        mkb,
        unit_key_ro: inf,
        samples: Vec::new(),
        volume_label: None,
    };
    // Zero duplicated fetch logic: the lib's `key_fetch` owns the
    // build-inputs-with-samples → ask-sources → return-keys flow. The CLI only
    // supplies the disc inputs and a way to (re)build its key source (the
    // `--key-url` OnlineSource).
    let make_sources: std::sync::Arc<
        dyn Fn() -> Vec<Box<dyn libfreemkv::keysource::KeySource>> + Send + Sync,
    > = std::sync::Arc::new(move || {
        vec![Box::new(freemkv_keysources::OnlineSource::new(
            url.clone(),
            auth.clone(),
        )) as Box<dyn libfreemkv::keysource::KeySource>]
    });
    Some(libfreemkv::keysource::key_fetch(inputs, make_sources))
}

/// How many encrypted aligned units to sample for key validation.
const SAMPLE_UNITS: usize = 4;

/// The keydb path to use: `--keydb <path>` if given; else the first
/// per-OS search location that exists (Windows `%APPDATA%\freemkv\keydb.cfg`
/// then the legacy `.config` dotfolder; Linux/macOS `~/.config/freemkv/keydb.cfg`),
/// else the canonical default location for that OS, else a bare `keydb.cfg`
/// in the cwd. The search/default policy lives in `freemkv-keysources`.
pub(crate) fn resolved_keydb_path(keydb_path: &Option<String>) -> std::path::PathBuf {
    keydb_path
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(freemkv_keysources::existing_keydb_path)
        .or_else(freemkv_keysources::default_keydb_path)
        .unwrap_or_else(|| std::path::PathBuf::from("keydb.cfg"))
}

/// Build the ordered `KeySource` list from the key flags, **local-first**:
///
/// - `--key-url` only → `[OnlineSource]` (no keydb consulted).
/// - `--keydb` only / neither → `[KeydbSource]` (the standard CLI behaviour;
///   "neither" still uses the default keydb location).
/// - both → `[KeydbSource, OnlineSource]` — a local keydb hit wins and never
///   makes a network round-trip; the service is the fallback.
///
/// `--key-url` is SSRF-validated (via the shared
/// [`freemkv_keysources::validate_keyserver_url`]) before the online source is
/// added; a rejected URL prints a warning and the online source is dropped (the
/// keydb, if any, still applies) rather than POSTing key material to an
/// internal/metadata host.
fn build_key_sources(
    keys: &KeyConfig,
    out: &Output,
) -> Vec<Box<dyn freemkv_keysources::KeySource>> {
    let mut sources: Vec<Box<dyn freemkv_keysources::KeySource>> = Vec::new();

    // Local keydb is added whenever the user didn't ask for online-only. (An
    // explicit --keydb, or no key flags at all, both want the keydb.)
    let online_only = keys.key_url.is_some() && keys.keydb_path.is_none();
    if !online_only {
        sources.push(Box::new(freemkv_keysources::KeydbSource::new(
            resolved_keydb_path(&keys.keydb_path),
        )));
    }

    if let Some(url) = &keys.key_url {
        match freemkv_keysources::validate_keyserver_url(url) {
            Ok(()) => sources.push(Box::new(freemkv_keysources::OnlineSource::new(
                url.clone(),
                keys.key_auth.clone().unwrap_or_default(),
            ))),
            Err(e) => {
                out.raw(
                    Normal,
                    &strings::fmt("error.keyserver_url_rejected", &[("error", &e)]),
                );
            }
        }
    }
    sources
}

/// Resolve an AACS key for a keyless-scanned `disc` from the configured sources
/// and apply it via `Disc::decrypt_with`. No-op for an unencrypted disc (no AACS
/// inputs). Each source hands its candidates out best-first and the shared loop
/// keeps the first whose key actually decrypts a `samples` unit (a wrong
/// candidate is rejected and the next tried). Sources are local-first — see
/// [`build_key_sources`].
fn apply_keys(disc: &mut libfreemkv::Disc, keys: &KeyConfig, samples: Vec<Vec<u8>>, out: &Output) {
    let Some(mut inputs) = disc.inputs() else {
        return; // not AACS-encrypted (or no inputs captured)
    };
    inputs.samples = samples;
    let sources = build_key_sources(keys, out);
    // The `_traced` variant resolves AND hands back the structured per-source
    // walk; each source's `get_uk` is tried in order and the first whose Unit
    // Keys validate against the samples is committed.
    let (_resolved, trace) =
        libfreemkv::keysource::resolve_and_apply_traced(&sources, &inputs, disc);
    // Render the structured walk to STDERR (never stdout — that may carry the
    // piped disc stream), suppressed only when quiet. English lives here in the
    // app layer; the library trace is typed enums only.
    if !out.is_quiet() {
        for line in render_resolution_trace(&trace) {
            eprintln!("{line}");
        }
    }
}

/// Render a [`libfreemkv::aacs::trace::ResolutionTrace`] into human-readable
/// `who > node > … > OUTCOME` lines — one per unlocker and per key source
/// consulted. The library trace is English-free typed enums; ALL English
/// mapping lives here in the app layer. Mirrors autorip's renderer (the two
/// apps are separate crates, so the mapping is duplicated, not shared).
fn render_resolution_trace(trace: &libfreemkv::aacs::trace::ResolutionTrace) -> Vec<String> {
    use libfreemkv::aacs::trace::{KeyNode, KeyOutcome as KO, UnlockOutcome};

    let mkb = |m: Option<u32>| match m {
        Some(n) => format!(" (MKBv{n})"),
        None => String::new(),
    };
    let mut lines = Vec::new();

    for step in &trace.unlock {
        // `who` is the unlocker's own name() — printed verbatim (no enum to map).
        let outcome = match step.outcome {
            UnlockOutcome::Unlocked => "UNLOCKED".to_string(),
            UnlockOutcome::FirmwareNotUnlockable => "firmware not unlockable".to_string(),
            UnlockOutcome::NoUsableHostCert { mkb: m } => format!("no usable host cert{}", mkb(m)),
            UnlockOutcome::CertRevoked { mkb: m } => format!("host cert revoked{}", mkb(m)),
            UnlockOutcome::HandshakeRejected => "handshake rejected".to_string(),
            UnlockOutcome::VidUnavailable => "Volume ID unavailable".to_string(),
        };
        lines.push(format!("unlock: {} > {outcome}", step.who));
    }

    for step in &trace.keys {
        // `who` is the source's own label() — printed verbatim (no enum to map).
        let nodes: Vec<&str> = step
            .path
            .iter()
            .map(|n| match n {
                KeyNode::MatchedDisc => "matched disc",
                KeyNode::NoEntry => "no entry",
                KeyNode::FoundUnitKeys => "found unit keys",
                KeyNode::FoundVuk => "found VUK",
                KeyNode::FoundMediaKey => "found media key",
                KeyNode::NeedVid => "need VID",
                KeyNode::VidFromUnlock => "VID from drive",
                KeyNode::VidFromKeydb => "VID from keydb",
                KeyNode::NoVid => "no VID",
                KeyNode::DerivedVuk => "derived VUK",
                KeyNode::DerivedUnitKeys => "derived unit keys",
            })
            .collect();
        let outcome = match step.outcome {
            KO::Resolved => "RESOLVED",
            KO::MissingVid => "MISSING VID",
            KO::NoKey => "NO KEY",
        };
        let mut parts = vec![step.who.clone()];
        parts.extend(nodes.into_iter().map(str::to_string));
        parts.push(outcome.to_string());
        lines.push(format!("key: {}", parts.join(" > ")));
    }

    lines
}

fn pipe_disc(
    source: &str,
    dest: &str,
    title_idx: usize,
    keys: &KeyConfig,
    raw: bool,
    _multipass: bool,
    out: &Output,
) -> Result<(), String> {
    let parsed = libfreemkv::parse_url(source);
    let device = match &parsed {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => p.clone(),
        _ => libfreemkv::find_drive()
            .map(|d| std::path::PathBuf::from(d.device_path()))
            .ok_or_else(|| strings::get("error.no_drive"))?,
    };

    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    let mut drive = libfreemkv::Drive::open(&device).map_err(|e| format!("{}", e))?;
    debug_drive_step("wait_ready", drive.wait_ready());
    debug_drive_step("init", drive.init());
    // probe_disc is advisory: it routinely fails (no disc, already probed) and
    // the scan below re-derives what it needs, so its result stays discarded.
    let _ = drive.probe_disc();
    // Lock the tray so the disc cannot eject mid-rip. The unlock is guaranteed
    // by `Drive::drop` (which calls `unlock_tray`): on every early-return path
    // below the local `drive` is dropped, and after it is moved into
    // `DiscStream` the boxed `Drive` is dropped when the stream is dropped on
    // any return. The only path that bypasses Drop is a SECOND Ctrl-C
    // (`_exit(130)`) — the first Ctrl-C now halts cleanly (loop check below)
    // and lets the stream drop, so the common interrupt case unlocks the tray.
    drive.lock_tray();

    let mut disc = libfreemkv::Disc::scan(&mut drive, &drive_scan_opts(keys.keydb_path()))
        .map_err(|e| format!("{}", e))?;
    // Sample encrypted units from the largest title to validate the resolved key
    // against real ciphertext before muxing.
    let samples = disc
        .titles
        .iter()
        .max_by_key(|t| t.size_bytes)
        .cloned()
        .map(|t| libfreemkv::read_encrypted_units(&mut drive, &t, SAMPLE_UNITS))
        .unwrap_or_default();
    apply_keys(&mut disc, keys, samples, out);

    if title_idx >= disc.titles.len() {
        return Err(strings::fmt(
            "error.title_out_of_range",
            &[
                ("num", &(title_idx + 1).to_string()),
                ("count", &disc.titles.len().to_string()),
            ],
        ));
    }

    // Pre-flight decrypt gate (disc-wide): catches a scrambled-but-uncracked
    // CSS disc (`css_error` set, `css` None — the content IS encrypted) and an
    // AACS disc with no resolved key, BEFORE building the stream. The per-title
    // (multi-VTS CSS) check runs again below once the chosen title's key is
    // resolved. `--raw` and unencrypted discs pass. Single verdict source as the
    // ISO mux path (`Disc::ensure_decryptable`).
    disc.ensure_decryptable(raw).map_err(|e| e.to_string())?;

    let title = disc.titles[title_idx].clone();
    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());

    // Per-title key resolution (Theme B fix, mirrors the ISO path's
    // per-title key block in libfreemkv mux/resolve.rs). The disc-wide
    // `decrypt_keys()`
    // carries the single key the scan cracked — but on a multi-VTS CSS DVD
    // a non-main-VTS `-t N` lives in a different VTS with a DIFFERENT
    // per-VTS title key, so the disc-wide key descrambles it to GARBAGE at
    // exit 0. `decrypt_keys_for_title` re-cracks from the chosen title's
    // own extents when it doesn't overlap the cracked span, and returns
    // `DecryptKeys::None` (never the wrong key) when that re-crack misses.
    //
    // The re-crack reads sectors off the live `drive` (a `SectorSource`),
    // which is still owned here — it is only moved into `DiscStream` below.
    // For AACS / single-VTS / unencrypted discs `decrypt_keys_for_title`
    // short-circuits to `decrypt_keys()`, so this is a no-op on those paths.
    // `_checked` also tells us whether the chosen title proved GENUINELY CLEAR
    // (an unencrypted stub in its own VTS on an otherwise-CSS disc): that title
    // needs no key, and the gate below must not false-error it with E7023.
    let (keys, title_is_clear) = disc.decrypt_keys_for_title_checked(title_idx, &mut drive, batch);

    // Per-title decrypt gate (mirrors the ISO mux path): on a multi-VTS CSS DVD
    // whose chosen title's VTS could not be re-cracked, `keys` is
    // `DecryptKeys::None` even though the disc-wide gate above passed; on an
    // AACS disc with no key it is also None. Either way, streaming that into
    // `DiscStream` passes ciphertext through unchanged → the demuxer sees no TS
    // syncs, emits nothing, and we'd write an empty/garbage MKV at exit 0. Fail
    // loudly with the same verdict source (`Disc::ensure_decryptable_keys`):
    // CssKeyMissing for CSS, NoDiscKey (named by hash) for AACS. `--raw` and
    // unencrypted discs pass.
    disc.ensure_title_decryptable(raw, &keys, title_is_clear)
        .map_err(|e| e.to_string())?;

    let format = disc.content_format;

    let mut input = libfreemkv::DiscStream::new(Box::new(drive), title, keys, batch, format);

    if raw {
        input.set_raw();
    }

    out.raw(Normal, &strings::get("rip.ok"));

    // From here, same as pipe(): headers → output → frame loop
    let mut buffered = Vec::new();
    while !input.headers_ready() {
        match input.read() {
            Ok(Some(frame)) => buffered.push(frame),
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    // The header loop breaks on EOF (`Ok(None)`) without re-checking
    // `headers_ready()`. If we drained the input before the video codec
    // parser emitted its codec_private (hvcC/avcC) — a damaged or very
    // short title — `codec_private()` returns `None` for the video track
    // and the muxer writes a track header with no CODEC_PRIVATE element.
    // The downstream zero-output guard does NOT catch this (one stray audio
    // PES byte clears it), so we would finalize a structurally-invalid MKV
    // and exit 0. Refuse here, mirroring autorip's run_mux gate.
    if !headers_resolved(input.headers_ready()) {
        return Err(libfreemkv::Error::MkvInvalid.to_string());
    }

    let info = input.info().clone();
    print_stream_info(out, &info);

    let mut title = info.clone();
    let disc_name = disc.meta_title.as_deref().unwrap_or(&disc.volume_id);
    title.playlist = disc_name.to_string();
    title.codec_privates = (0..info.streams.len())
        .map(|i| input.codec_private(i))
        .collect();

    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", dest)]));
    let raw_output = match libfreemkv::output(dest, &title) {
        Ok(s) => {
            out.raw(Normal, &strings::get("rip.ok"));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };
    let mut output = libfreemkv::pes::CountingStream::new(raw_output);

    out.blank(Normal);

    let total_bytes = info.size_bytes;
    let start = std::time::Instant::now();
    let mut last_update = start;

    for frame in &buffered {
        output.write(frame).map_err(|e| format!("{}", e))?;
    }

    let mut interrupted = false;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            interrupted = true;
            break;
        }

        match input.read() {
            Ok(Some(frame)) => {
                output.write(&frame).map_err(|e| format!("{}", e))?;

                let now = std::time::Instant::now();
                if !out.is_quiet() && now.duration_since(last_update).as_secs_f64() >= 0.5 {
                    print_progress(output.bytes_written(), total_bytes, &start);
                    last_update = now;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    // On interrupt do NOT finalize: a SIGINT mid-mux leaves a truncated file.
    // Calling `output.finish()` + returning Ok would write the container footer
    // and report success, presenting a partial MKV as complete (exit 0). Bail
    // with an error so the exit code is non-zero and we don't claim success.
    // Re-read the flag here too: a SIGINT that lands during the final
    // `input.read()` (the one returning `Ok(None)`) breaks the loop without
    // tripping the top-of-loop check, so the in-loop `interrupted` can be stale.
    if mux_was_interrupted(interrupted, INTERRUPTED.load(Ordering::SeqCst)) {
        return Err(interrupted_error(out));
    }

    // Zero-output guard (Theme A): a natural drain that wrote no streams / no
    // frame bytes must NOT be finalized and reported "Complete" — that is the
    // empty/garbage-output silent failure (undecryptable input → demuxer emits
    // nothing). Surface `Error::NoStreams` (as the ISO path does via the
    // NoStreams gate in libfreemkv mux/resolve.rs) so the exit code is nonzero
    // and the user sees a localized message instead of a header-only "success".
    if !mux_produced_output(info.streams.len(), output.bytes_written()) {
        return Err(libfreemkv::Error::NoStreams.to_string());
    }

    output.finish().map_err(|e| format!("{}", e))?;

    print_completion_summary(out, output.bytes_written(), start);
    Ok(())
}

/// Minimum plausible PES-frame payload for a non-empty mux. `CountingStream`
/// counts only the bytes of `PesFrame.data` actually handed to the sink, so a
/// successful mux of even one tiny audio frame clears this. A value of 0 means
/// the frame loop drained on the first `Ok(None)` having written nothing —
/// the symptom of undecryptable/empty input (no TS syncs → demuxer emits
/// nothing). We require strictly more than zero rather than a header threshold
/// because `bytes_written()` is frame-payload bytes (not container bytes), so
/// even a 1-byte payload is real media and any container header is not counted.
const MIN_MUX_PAYLOAD_BYTES: u64 = 1;

/// Whether a completed mux actually produced output, the guard both pipe paths
/// run before declaring success (Theme A). A "natural drain" (`Ok(None)` on the
/// first read) followed by `output.finish()` + a "Complete" summary must NOT be
/// reported as success when the title carried no streams OR not a single frame
/// payload byte reached the sink — that is the zero-output / undecryptable-input
/// silent failure. Returns `false` (→ caller errors, nonzero exit) in those
/// cases; `true` only when there is at least one stream and ≥1 payload byte.
fn mux_produced_output(num_streams: usize, bytes_written: u64) -> bool {
    num_streams > 0 && bytes_written >= MIN_MUX_PAYLOAD_BYTES
}

/// Whether a completed disc→ISO sweep actually recovered any readable data,
/// the guard `disc_to_iso` runs before declaring success — the sweep-path
/// analogue of `mux_produced_output`. `Disc::copy` returns `Ok` even when every
/// ECC block was unreadable and zero-filled (whole disc unreadable): the ISO on
/// disk is all zeroes and unusable. Returns `false` (→ caller prints `rip.no_data`
/// and exits non-zero) when nothing readable came off the disc; `true` only when
/// at least one byte was recovered.
fn disc_copy_recovered_data(bytes_good: u64) -> bool {
    bytes_good > 0
}

/// The header-resolution gate both pipe paths run after their
/// `while !input.headers_ready()` loop. That loop breaks on EOF (`Ok(None)`)
/// without re-checking, so a disc with damaged video sectors or a very short
/// title can drain before the video codec parser emits its codec_private
/// (hvcC/avcC). Muxing then writes a track header with no CODEC_PRIVATE
/// element — a structurally-invalid MKV that the zero-output guard does NOT
/// catch (one stray audio PES byte clears it). Returns `false` (→ caller
/// errors with `MkvInvalid`, nonzero exit) when headers never resolved;
/// `true` only when `headers_ready()` actually became true.
fn headers_resolved(headers_ready: bool) -> bool {
    headers_ready
}

/// Print the interrupt notice and return the error string both pipe paths use
/// when a SIGINT lands mid-mux. The message names the output as incomplete so
/// the user knows not to trust it.
fn interrupted_error(out: &Output) -> String {
    out.blank(Normal);
    out.raw(Normal, &strings::get("error.interrupted_incomplete"));
    strings::get("rip.interrupted")
}

/// Decide whether a per-title mux failure should abort the WHOLE rip (fatal) or
/// be skipped with a warning so the remaining titles still mux.
///
/// Core principle: **NO FALSE ERRORS, and a failure in one extra title must not
/// kill the whole rip.** When `freemkv iso://X mkv://dir/` muxes ALL titles, one
/// incidental copy-protected-but-uncrackable stub (a 0.5 s menu loop, an
/// FBI-warning title) used to abort the entire mux with `E7023` and make users
/// think freemkv was broken. It must instead skip that title and finish the rest.
///
/// A failure is FATAL (hard error, non-zero exit) when ANY of:
/// - the error is NOT a skippable per-title stub failure. Two codes are
///   skippable: `E7023` / [`libfreemkv::Error::CssKeyMissing`] (copy-protected,
///   no key recoverable) and `E6008` / [`libfreemkv::Error::MkvInvalid`] (the
///   title produced no muxable frames — an empty nav/menu PGC stub). Every other
///   error (IO, NoDiscKey/E7022 AACS, …) is a real problem and stays fatal;
/// - the user explicitly selected titles with `-t N` (`explicit_selection`) —
///   the title they asked for failing is exactly what they need to know about;
/// - this title IS the main feature (`is_feature`, title index 0) — the movie
///   itself failing is never "incidental";
/// - the rip targets a single title (`!multi_title`) — there is no "other title"
///   to carry on with, so the lone failure is the result.
///
/// It is SKIPPABLE (warn + continue, command may still exit 0) ONLY when an
/// all-titles rip (`multi_title && !explicit_selection`) hits a non-feature
/// title whose failure is one of the skippable stub codes. This runtime
/// classification is the operative mechanism for empty nav/menu PGC stubs:
/// rather than pre-filtering the job set, a title that scans as non-empty yet
/// still muxes to nothing (E6008) is caught here at rip time and skipped with a
/// clear notice (`rip.title_skipped_empty`) instead of printing a scary error.
fn is_title_failure_fatal(
    err: &str,
    multi_title: bool,
    explicit_selection: bool,
    is_feature: bool,
) -> bool {
    // Only a per-title STUB failure is ever a candidate for skipping:
    // E7023 (CssKeyMissing — copy-protected) or E6008 (MkvInvalid — empty/no
    // frames). Anything else is always fatal. The error string is libfreemkv's
    // `E<code>[: <data>]` Display form (see `parse_error_code`).
    let is_skippable_code =
        parse_error_code(err).is_some_and(|(code, _)| matches!(code, "E7023" | "E6008"));
    if !is_skippable_code {
        return true;
    }
    // A stub failure on the feature, an explicitly-requested title, or in a
    // single-title rip is the title the user actually wants — hard error.
    is_feature || explicit_selection || !multi_title
}

/// One title: open input, open output, stream PES frames.
/// Used for non-disc sources (ISO, MKV, M2TS, network, stdio).
fn pipe(
    source: &str,
    dest: &str,
    opts: &libfreemkv::InputOptions,
    out: &Output,
) -> Result<(), String> {
    // Open input
    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    let mut input = match libfreemkv::input(source, opts) {
        Ok(s) => {
            out.raw(Normal, &strings::get("rip.ok"));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };

    // Read frames until codec headers are ready (also parses metadata headers for stdio/network)
    let mut buffered = Vec::new();
    while !input.headers_ready() {
        match input.read() {
            Ok(Some(frame)) => buffered.push(frame),
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    // The header loop breaks on EOF (`Ok(None)`) without re-checking
    // `headers_ready()`. If the input drained before the video codec parser
    // emitted its codec_private (hvcC/avcC), `codec_private()` yields `None`
    // for the video track and the muxer writes a track header with no
    // CODEC_PRIVATE element — a structurally-invalid MKV. The zero-output
    // guard below does NOT catch this (one stray audio PES byte clears it),
    // so without this check we would finalize the broken file and exit 0.
    // Refuse here, mirroring autorip's run_mux gate.
    if !headers_resolved(input.headers_ready()) {
        return Err(libfreemkv::Error::MkvInvalid.to_string());
    }

    // Get info after header scanning (stdio/network populate info during read)
    let info = input.info().clone();
    print_stream_info(out, &info);

    // Build output title with codec_privates from input
    let mut title = info.clone();
    title.codec_privates = (0..info.streams.len())
        .map(|i| input.codec_private(i))
        .collect();

    // Open output, wrapped with byte counter for progress
    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", dest)]));
    let raw_output = match libfreemkv::output(dest, &title) {
        Ok(s) => {
            out.raw(Normal, &strings::get("rip.ok"));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };
    let mut output = libfreemkv::pes::CountingStream::new(raw_output);

    out.blank(Normal);

    let total_bytes = info.size_bytes;
    let start = std::time::Instant::now();
    let mut last_update = start;

    // Write buffered frames
    for frame in &buffered {
        output.write(frame).map_err(|e| format!("{}", e))?;
    }

    // Stream remaining frames
    let mut interrupted = false;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            interrupted = true;
            break;
        }

        match input.read() {
            Ok(Some(frame)) => {
                output.write(&frame).map_err(|e| format!("{}", e))?;

                let now = std::time::Instant::now();
                if !out.is_quiet() && now.duration_since(last_update).as_secs_f64() >= 0.5 {
                    print_progress(output.bytes_written(), total_bytes, &start);
                    last_update = now;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    // See `pipe_disc`: a SIGINT mid-mux must not finalize a truncated file as
    // success. Re-read the flag so a SIGINT during the final read (which breaks
    // the loop via `Ok(None)` without hitting the top-of-loop check) is caught.
    if mux_was_interrupted(interrupted, INTERRUPTED.load(Ordering::SeqCst)) {
        return Err(interrupted_error(out));
    }

    // Zero-output guard (Theme A): a natural drain that wrote no streams / no
    // frame bytes must NOT be finalized and reported "Complete" — that is the
    // empty/garbage-output silent failure (undecryptable input → demuxer emits
    // nothing). Surface `Error::NoStreams` (as the ISO path does via the
    // NoStreams gate in libfreemkv mux/resolve.rs) so the exit code is nonzero
    // and the user sees a localized message instead of a header-only "success".
    if !mux_produced_output(info.streams.len(), output.bytes_written()) {
        return Err(libfreemkv::Error::NoStreams.to_string());
    }

    output.finish().map_err(|e| format!("{}", e))?;

    print_completion_summary(out, output.bytes_written(), start);
    Ok(())
}

// ── Disc → ISO (raw sector copy, not a stream) ────────────────────────────

/// Returns true on success, false on any failure (no drive, scan error,
/// `Disc::copy` error). The caller propagates this to `main`'s exit code so a
/// scripted `$?` check sees the failure.
fn disc_to_iso(
    source: &str,
    dest: &str,
    keys: &KeyConfig,
    raw: bool,
    multipass: bool,
    out: &Output,
) -> bool {
    let parsed_source = libfreemkv::parse_url(source);
    let parsed_dest = libfreemkv::parse_url(dest);
    let device = match &parsed_source {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => Some(p.clone()),
        _ => None,
    };

    let mut drive = match device {
        Some(ref d) => match libfreemkv::Drive::open(d) {
            Ok(d) => d,
            Err(e) => {
                out.raw(Normal, &render_error(&e));
                return false;
            }
        },
        None => match libfreemkv::find_drive() {
            Some(d) => d,
            None => {
                out.raw(Normal, &strings::get("error.no_drive"));
                return false;
            }
        },
    };
    out.raw(
        Normal,
        &strings::fmt("rip.drive", &[("device", drive.device_path())]),
    );
    debug_drive_step("wait_ready", drive.wait_ready());
    debug_drive_step("init", drive.init());
    // probe_disc is advisory: it routinely fails (no disc, already probed) and
    // the scan below re-derives what it needs, so its result stays discarded.
    let _ = drive.probe_disc();

    let mut disc = match libfreemkv::Disc::scan(&mut drive, &drive_scan_opts(keys.keydb_path())) {
        Ok(d) => d,
        Err(e) => {
            out.raw(
                Normal,
                &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
            );
            return false;
        }
    };
    // Resolve + apply the AACS key so the keys persist in the mapfile during
    // disc→ISO copy (the mux step reads them back to decrypt). Sample encrypted
    // units first so the resolved key is validated against real ciphertext.
    let samples = disc
        .titles
        .iter()
        .max_by_key(|t| t.size_bytes)
        .cloned()
        .map(|t| libfreemkv::read_encrypted_units(&mut drive, &t, SAMPLE_UNITS))
        .unwrap_or_default();
    apply_keys(&mut disc, keys, samples, out);

    // Pre-flight decrypt gate: a decrypting disc→ISO copy (not --raw) of an
    // encrypted disc with no usable key would write ciphertext to the ISO and
    // exit 0. Refuse here — right after scan + key resolution, BEFORE locking
    // the tray, sizing the ISO, or reading a single sector — so the failure is
    // pre-flight with no partial ISO. (`Disc::copy` enforces the same gate
    // internally; this surfaces it earlier with the localized message.) --raw
    // and unencrypted discs pass.
    if let Err(e) = disc.ensure_decryptable(raw) {
        out.raw(Normal, &render_error(&e));
        return false;
    }

    let disc_name = sanitize_name(disc.meta_title.as_deref().unwrap_or(&disc.volume_id));
    let (iso_path, is_null) = match &parsed_dest {
        libfreemkv::StreamUrl::Iso { path } => (path.clone(), false),
        libfreemkv::StreamUrl::Null => {
            let p = std::path::PathBuf::from("/dev/null");
            (p, true)
        }
        _ => unreachable!(),
    };

    let total_bytes = disc.capacity_sectors as u64 * libfreemkv::consts::SECTOR_BYTES_U64;
    out.raw(
        Normal,
        &strings::fmt(
            "rip.disc_label",
            &[
                ("name", &disc_name),
                (
                    "size",
                    &format!("{:.1}", total_bytes as f64 / 1_073_741_824.0),
                ),
            ],
        ),
    );
    if !is_null {
        out.raw(
            Normal,
            &strings::fmt("rip.output", &[("path", &iso_path.display().to_string())]),
        );
    }
    out.blank(Normal);

    drive.lock_tray();
    let start = std::time::Instant::now();
    let last_update = std::cell::Cell::new(start);
    let last_work_done = std::cell::Cell::new(None::<u64>);
    let last_speed_time = std::cell::Cell::new(start);

    struct CliProgress<'a> {
        out: &'a Output,
        last_update: &'a std::cell::Cell<std::time::Instant>,
        last_work_done: &'a std::cell::Cell<Option<u64>>,
        last_speed_time: &'a std::cell::Cell<std::time::Instant>,
    }
    impl libfreemkv::progress::Progress for CliProgress<'_> {
        fn report(&self, p: &libfreemkv::progress::PassProgress) -> bool {
            if !self.out.is_quiet() {
                let now = std::time::Instant::now();
                if now.duration_since(self.last_update.get()).as_secs_f64() >= 0.5 {
                    self.last_update.set(now);

                    let inst_speed = match self.last_work_done.get() {
                        Some(prev) => {
                            let prev_time = self.last_speed_time.get();
                            let dt = now.duration_since(prev_time).as_secs_f64();
                            if dt > 0.0 {
                                (p.work_done.saturating_sub(prev) as f64 / 1_048_576.0) / dt
                            } else {
                                0.0
                            }
                        }
                        None => 0.0,
                    };
                    self.last_work_done.set(Some(p.work_done));
                    self.last_speed_time.set(now);

                    print_disc_progress(p, inst_speed);
                }
            }
            // Returning false halts the copy. Consult the global SIGINT flag so
            // the FIRST Ctrl-C cleanly stops the sweep and lets `unlock_tray()`
            // run below — instead of being ignored until a second Ctrl-C forces
            // `_exit(130)`, which bypasses tray unlock entirely. (The previous
            // `halt` Arc was wired to a value nothing ever stored into — dead.)
            copy_should_continue(INTERRUPTED.load(Ordering::SeqCst))
        }
    }
    let progress = CliProgress {
        out,
        last_update: &last_update,
        last_work_done: &last_work_done,
        last_speed_time: &last_speed_time,
    };

    let copy_opts = libfreemkv::disc::CopyOptions {
        decrypt: !raw,
        multipass,
        halt: None,
        progress: Some(&progress),
        ..Default::default()
    };
    let success = match disc.copy(&mut drive, &iso_path, &copy_opts) {
        Ok(r) if r.halted => {
            // Ctrl-C halted the copy (report() returned false). Don't print
            // "Complete" over a partial ISO — say it was interrupted and report
            // failure so the exit code is non-zero. The mapfile is preserved, so
            // a later run can resume.
            if !out.is_quiet() {
                eprint!("\r\x1b[K");
            }
            out.raw(Normal, &strings::get("rip.interrupted"));
            false
        }
        Ok(r) if !disc_copy_recovered_data(r.bytes_good) => {
            // The copy ran to completion but recovered ZERO readable bytes —
            // every ECC block was zero-filled and marked NonTrimmed (whole disc
            // unreadable). The ISO on disk is all zeroes and unusable. Don't
            // print "Complete" or return success: a scripted caller checking $?
            // must see a non-zero exit, mirroring the NoStreams guard on the
            // mux paths in this file.
            if !out.is_quiet() {
                eprint!("\r\x1b[K");
            }
            let mb_bad = r.bytes_unreadable as f64 / 1_048_576.0;
            out.raw(
                Normal,
                &strings::fmt("rip.no_data", &[("unreadable", &format!("{mb_bad:.1}"))]),
            );
            false
        }
        Ok(r) => {
            if !out.is_quiet() {
                eprint!("\r\x1b[K");
            }
            let elapsed = start.elapsed().as_secs_f64();
            let mb = r.bytes_total as f64 / (1024.0 * 1024.0);
            let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
            out.raw(
                Normal,
                &strings::fmt(
                    "rip.complete",
                    &[
                        ("size", &format!("{:.1}", mb / 1024.0)),
                        ("unit", "GB"),
                        ("time", &format!("{elapsed:.0}")),
                        ("speed", &format!("{speed:.0}")),
                    ],
                ),
            );
            if multipass {
                let gb_good = r.bytes_good as f64 / 1_073_741_824.0;
                let mb_bad = r.bytes_unreadable as f64 / 1_048_576.0;
                let mb_pending = r.bytes_pending as f64 / 1_048_576.0;
                let mapfile_path = disc.mapfile_for(&iso_path);
                let main_title = disc.titles.first();
                let main_title_bad = main_title
                    .map(|t| disc.bytes_bad_in_title(&mapfile_path, t))
                    .unwrap_or(0);
                // Report damage as a MAIN-TITLE duration only. The previous
                // disc-wide figure multiplied a whole-disc bad-byte ratio by
                // `disc_dur` — but `disc_dur` is only the FIRST title's runtime,
                // so once bonus content makes the disc larger than the main
                // title the product was dimensionally wrong (bad MB scaled by the
                // wrong duration). Scale the main title's bad bytes by its OWN
                // size and runtime; the raw unreadable/pending MB above still
                // surfaces any loss that falls outside the main title.
                let main_lost_secs = main_title
                    .map(|t| (t.size_bytes, t.duration_secs))
                    .filter(|&(sz, dur)| main_title_bad > 0 && sz > 0 && dur > 0.0)
                    .map(|(sz, dur)| main_title_bad as f64 / sz as f64 * dur)
                    .unwrap_or(0.0);
                out.raw(
                    Normal,
                    &strings::fmt(
                        "rip.mapfile_summary",
                        &[
                            ("good", &format!("{gb_good:.2}")),
                            ("unreadable", &format!("{mb_bad:.1}")),
                            ("pending", &format!("{mb_pending:.1}")),
                        ],
                    ),
                );
                if main_lost_secs > 0.0 {
                    let main_str = fmt_damage_time(main_lost_secs);
                    out.raw(
                        Normal,
                        &strings::fmt("rip.damage_lost_movie", &[("time", &main_str)]),
                    );
                }
            }
            true
        }
        Err(e) => {
            out.raw(Normal, &render_error(&e));
            false
        }
    };

    drive.unlock_tray();
    success
}

// ── dir:// decrypted file-tree extraction ───────────────────────────────────

/// Extract a disc's decrypted file tree to a host directory (`dir://`). Routed
/// here (before the generic mux path) for a `dir://` dest with a disc-source
/// input (`disc://` or `iso://`). 1-shot, decrypt-only — recovery for damaged
/// media is the `disc→iso --multipass` then `iso→dir` workflow. Returns true on
/// success (a fully-clean tree); a lossy extraction prints the per-file summary
/// and returns false (→ non-zero exit) so a script can re-run via the ISO path.
fn dir_to_extract(
    source: &str,
    dest: &str,
    keys: &KeyConfig,
    parsed_source: &libfreemkv::StreamUrl,
    force: bool,
    out: &Output,
) -> bool {
    let dest_path = match libfreemkv::parse_url(dest) {
        libfreemkv::StreamUrl::Dir { path } => path,
        _ => return false,
    };

    // Open the right reader + scan, resolving keys, then extract. The two
    // source kinds differ only in how the `SectorSource` + scanned `Disc` are
    // obtained; the extraction is identical once keys are resolved.
    match parsed_source {
        libfreemkv::StreamUrl::Disc { device } => {
            let mut drive = match device {
                Some(p) => match libfreemkv::Drive::open(p) {
                    Ok(d) => d,
                    Err(e) => {
                        out.raw(Normal, &render_error(&e));
                        return false;
                    }
                },
                None => match libfreemkv::find_drive() {
                    Some(d) => d,
                    None => {
                        out.raw(Normal, &strings::get("error.no_drive"));
                        return false;
                    }
                },
            };
            out.raw(
                Normal,
                &strings::fmt("rip.drive", &[("device", drive.device_path())]),
            );
            debug_drive_step("wait_ready", drive.wait_ready());
            debug_drive_step("init", drive.init());
            let _ = drive.probe_disc();

            let mut disc =
                match libfreemkv::Disc::scan(&mut drive, &drive_scan_opts(keys.keydb_path())) {
                    Ok(d) => d,
                    Err(e) => {
                        out.raw(
                            Normal,
                            &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
                        );
                        return false;
                    }
                };
            let samples = disc
                .titles
                .iter()
                .max_by_key(|t| t.size_bytes)
                .cloned()
                .map(|t| libfreemkv::read_encrypted_units(&mut drive, &t, SAMPLE_UNITS))
                .unwrap_or_default();
            apply_keys(&mut disc, keys, samples, out);
            if let Err(e) = disc.ensure_decryptable(false) {
                out.raw(Normal, &render_error(&e));
                return false;
            }
            drive.lock_tray();
            let ok = run_extract(&disc, &mut drive, &dest_path, force, out);
            drive.unlock_tray();
            ok
        }
        libfreemkv::StreamUrl::Iso { path } => {
            let mut reader = match libfreemkv::FileSectorSource::open(path) {
                Ok(r) => r,
                Err(e) => {
                    out.raw(
                        Normal,
                        &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
                    );
                    return false;
                }
            };
            let capacity =
                <libfreemkv::FileSectorSource as libfreemkv::SectorSource>::capacity_sectors(
                    &reader,
                );
            let mut disc =
                match libfreemkv::Disc::scan_image(&mut reader, capacity, &keyless_scan_opts()) {
                    Ok(d) => d,
                    Err(e) => {
                        out.raw(
                            Normal,
                            &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
                        );
                        return false;
                    }
                };
            let samples = disc
                .titles
                .iter()
                .max_by_key(|t| t.size_bytes)
                .cloned()
                .map(|t| libfreemkv::read_encrypted_units(&mut reader, &t, SAMPLE_UNITS))
                .unwrap_or_default();
            apply_keys(&mut disc, keys, samples, out);
            if let Err(e) = disc.ensure_decryptable(false) {
                out.raw(Normal, &render_error(&e));
                return false;
            }
            run_extract(&disc, &mut reader, &dest_path, force, out)
        }
        _ => {
            // Unreachable: preflight rejects non-disc sources for dir://.
            out.raw(
                Normal,
                &strings::fmt("error.dir_source_unsupported", &[("source", source)]),
            );
            false
        }
    }
}

/// Run `Disc::extract_tree` and render the result. Shared by the disc:// and
/// iso:// `dir://` paths.
fn run_extract(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    dest_path: &std::path::Path,
    force: bool,
    out: &Output,
) -> bool {
    out.raw(
        Normal,
        &strings::fmt(
            "dir.extracting",
            &[("path", &dest_path.display().to_string())],
        ),
    );
    out.blank(Normal);

    // Bridge the CLI's SIGINT flag into a libfreemkv Halt the producer polls
    // at file / batch boundaries. A watcher thread flips the halt when SIGINT
    // arrives mid-extraction so a long extract stops promptly (the producer
    // leaves the in-flight file as `.partial`, never a half-written file that
    // looks complete). The watcher exits when extraction finishes (via `done`).
    let halt = libfreemkv::Halt::new();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if INTERRUPTED.load(Ordering::SeqCst) {
        halt.cancel();
    }
    let watcher = {
        let halt = halt.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    halt.cancel();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        })
    };

    // Signal `done` and join the watcher from a Drop guard so the watcher is
    // ALWAYS released — including the unwind path. `extract_tree` is not
    // panic-free (a slice/arithmetic bug could panic); without the guard a
    // panic would skip the `done.store`/`join` below and leave the watcher
    // spinning its 200 ms poll until process exit. The guard's Drop runs on
    // both normal return and unwind, so the thread is signalled and reaped
    // either way.
    struct WatcherGuard {
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for WatcherGuard {
        fn drop(&mut self) {
            self.done.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
    let _watcher_guard = WatcherGuard {
        done: done.clone(),
        handle: Some(watcher),
    };

    let opts = libfreemkv::ExtractOptions {
        force,
        progress: None,
        halt: Some(halt.clone()),
    };

    let outcome = disc.extract_tree(reader, dest_path, &opts);
    // `_watcher_guard`'s Drop (at end of scope, or on unwind) signals `done`
    // and joins the watcher; no explicit store/join needed here.

    match outcome {
        Ok(res) => {
            // Per-file loss lines (only the lossy ones, to keep output terse).
            for f in &res.files {
                let lost = f.bytes_unreadable;
                if lost > 0 {
                    out.raw(
                        Normal,
                        &strings::fmt(
                            "dir.file_lossy",
                            &[
                                ("file", &f.path.display().to_string()),
                                ("lost", &format!("{:.2}", lost as f64 / 1_048_576.0)),
                            ],
                        ),
                    );
                }
            }
            if res.halted {
                out.raw(Normal, &strings::get("rip.interrupted"));
                return false;
            }
            let good_mb = res.bytes_good as f64 / 1_048_576.0;
            if res.complete {
                out.raw(
                    Normal,
                    &strings::fmt(
                        "dir.complete",
                        &[
                            ("files", &res.files.len().to_string()),
                            ("size", &format!("{good_mb:.1}")),
                        ],
                    ),
                );
                true
            } else {
                let lost_mb = res.bytes_lost() as f64 / 1_048_576.0;
                out.raw(
                    Normal,
                    &strings::fmt(
                        "dir.lossy",
                        &[
                            ("files", &res.files.len().to_string()),
                            ("lost", &format!("{lost_mb:.2}")),
                        ],
                    ),
                );
                // A lossy extraction returns failure (non-zero exit) so scripts
                // can detect "extracted but holed" and re-run via iso multipass.
                false
            }
        }
        Err(e) => {
            out.raw(Normal, &render_error(&e));
            false
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn fmt_speed(mbps: f64) -> String {
    if mbps >= 1.0 {
        format!("{:.1} MB/s", mbps)
    } else if mbps * 1024.0 >= 1.0 {
        format!("{:.0} KB/s", mbps * 1024.0)
    } else if mbps > 0.0 {
        format!("{:.0} B/s", mbps * 1_048_576.0)
    } else {
        "stalled".into()
    }
}

fn fmt_eta(secs: f64) -> String {
    if secs <= 0.0 || secs.is_infinite() {
        return "?:??".into();
    }
    let h = secs as u64 / 3600;
    let m = (secs as u64 % 3600) / 60;
    let s = secs as u64 % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

fn fmt_damage_time(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else if secs >= 1.0 {
        format!("{:.0}s", secs)
    } else if secs >= 0.01 {
        format!("{:.2}s", secs)
    } else {
        format!("{:.0}ms", secs * 1000.0)
    }
}

/// Render the disc-level damage string ("lost" / "no loss") for the live
/// progress line.
///
/// "Lost" means READ FAILED only: `bytes_unreadable_total` (gave up) plus
/// `bytes_retryable_total` (failed, awaiting retry — NonTrimmed/NonScraped).
/// It deliberately does NOT include `bytes_pending_total`, which also folds
/// in not-yet-attempted (NonTried) sectors — counting those would make a
/// healthy in-progress rip report most of its remaining runtime as "lost".
/// The title-level path (`bytes_bad_in_main_title`) is already failed-only.
fn fmt_disc_damage(p: &libfreemkv::progress::PassProgress) -> String {
    let bytes_disc = p.bytes_total_disc;
    if bytes_disc == 0 {
        return strings::get("rip.damage_none");
    }
    let bytes_failed = p
        .bytes_unreadable_total
        .saturating_add(p.bytes_retryable_total);
    let disc_damage_secs = if bytes_failed > 0 {
        p.disc_duration_secs
            .filter(|&d| d > 0.0)
            .map(|dur| bytes_failed as f64 / bytes_disc as f64 * dur)
            .unwrap_or(0.0)
    } else {
        0.0
    };
    let title_damage_secs = if p.bytes_bad_in_main_title > 0 {
        p.main_title_duration_secs
            .zip(p.main_title_size_bytes)
            .filter(|&(dur, sz)| dur > 0.0 && sz > 0)
            .map(|(dur, sz)| p.bytes_bad_in_main_title as f64 / sz as f64 * dur)
    } else {
        None
    };

    if bytes_failed > 0 {
        let disc_str = fmt_damage_time(disc_damage_secs);
        match title_damage_secs {
            Some(ms) if ms > 0.0 && ms < disc_damage_secs * 0.99 => strings::fmt(
                "rip.damage_lost",
                &[("time", &disc_str), ("movie_time", &fmt_damage_time(ms))],
            ),
            Some(_) | None => strings::fmt("rip.damage_lost_movie", &[("time", &disc_str)]),
        }
    } else {
        strings::get("rip.damage_none")
    }
}

fn print_disc_progress(p: &libfreemkv::progress::PassProgress, inst_speed_mbps: f64) {
    let bytes_disc = p.bytes_total_disc;
    if bytes_disc == 0 {
        return;
    }
    // For Patch modes (Trim/Scrape), show work_done/work_total percentage.
    // bytes_good_total doesn't advance until sectors are recovered, leaving
    // progress stuck at 0% even though patch is working through bad ranges.
    let gb_done = match p.kind {
        libfreemkv::progress::PassKind::Sweep | libfreemkv::progress::PassKind::Mux => {
            p.work_done as f64 / 1_073_741_824.0
        }
        libfreemkv::progress::PassKind::Trim { .. }
        | libfreemkv::progress::PassKind::Scrape { .. } => {
            // Show progress through bad ranges, not just recovered data
            let pct = p.work_pct();
            (pct / 100.0) * (bytes_disc as f64 / 1_073_741_824.0)
        }
        _ => p.bytes_good_total as f64 / 1_073_741_824.0,
    };
    let gb_total = bytes_disc as f64 / 1_073_741_824.0;
    // `work_pct()` guards `work_total == 0` (returns 100.0) so an empty pass
    // can't produce a `NaN%`. Patch modes (Trim/Scrape) show progress through
    // bad ranges; Sweep/Mux show work_done/work_total — same formula either way.
    let pct = p.work_pct();
    let eta = if inst_speed_mbps > 0.01 && p.work_total > p.work_done {
        let remaining_mb = (p.work_total - p.work_done) as f64 / 1_048_576.0;
        fmt_eta(remaining_mb / inst_speed_mbps)
    } else {
        "?:??".into()
    };
    let damage = fmt_disc_damage(p);
    eprint!(
        "\r  {:.1}/{:.1} GB ({:.1}%)  {}  ETA {}    {}    ",
        gb_done,
        gb_total,
        pct,
        fmt_speed(inst_speed_mbps),
        eta,
        damage,
    );
    let _ = std::io::stderr().flush();
}

fn print_progress(done: u64, total: u64, start: &std::time::Instant) {
    let elapsed = start.elapsed().as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }
    let mb_done = done as f64 / 1_048_576.0;
    let avg = mb_done / elapsed;

    if total > 0 {
        let pct = (done as f64 / total as f64 * 100.0).min(100.0);
        let mb_total = total as f64 / 1_048_576.0;
        let eta = if avg > 0.0 {
            // `done` can exceed `total` (container overhead vs source-reported
            // size); saturate so the remaining-bytes math never underflows.
            let s = total.saturating_sub(done) as f64 / 1_048_576.0 / avg;
            format!("{}:{:02}", s as u64 / 60, s as u64 % 60)
        } else {
            "?:??".into()
        };
        if mb_total >= 1024.0 {
            eprint!(
                "\r  {:.1} GB / {:.1} GB  ({:.1}%)  {:.1} MB/s  ETA {}    ",
                mb_done / 1024.0,
                mb_total / 1024.0,
                pct,
                avg,
                eta
            );
        } else {
            eprint!(
                "\r  {:.0} MB / {:.0} MB  ({:.1}%)  {:.1} MB/s  ETA {}    ",
                mb_done, mb_total, pct, avg, eta
            );
        }
    } else {
        eprint!("\r  {:.1} MB  {:.1} MB/s    ", mb_done, avg);
    }
    let _ = std::io::stderr().flush();
}

/// Log a discarded drive-handshake step error to stderr (debug-grade). These
/// steps (`wait_ready`, `init`) are best-effort — the subsequent scan re-derives
/// what it needs — but a failure here is a useful breadcrumb when a later scan
/// fails, so surface it instead of silently dropping it. The common Ok path is
/// silent.
fn debug_drive_step(step: &str, result: libfreemkv::Result<()>) {
    if let Err(e) = result {
        eprintln!("freemkv: drive {step} (advisory) failed: {e}");
    }
}

/// Clear the progress line and print the final `rip.complete` summary. Shared
/// by `pipe_disc` and `pipe` (identical tail). `\r\x1b[K` erases from the cursor
/// to end of line, so it adapts to any progress-line width instead of relying on
/// a fixed run of spaces.
fn print_completion_summary(out: &Output, done: u64, start: std::time::Instant) {
    if !out.is_quiet() {
        eprint!("\r\x1b[K");
    }
    let elapsed = start.elapsed().as_secs_f64();
    let mb = done as f64 / (1024.0 * 1024.0);
    let (sz, unit) = if mb >= 1024.0 {
        (mb / 1024.0, "GB")
    } else {
        (mb, "MB")
    };
    let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
    out.raw(
        Normal,
        &strings::fmt(
            "rip.complete",
            &[
                ("size", &format!("{sz:.1}")),
                ("unit", unit),
                ("time", &format!("{elapsed:.0}")),
                ("speed", &format!("{speed:.0}")),
            ],
        ),
    );
}

fn print_stream_info(out: &Output, meta: &libfreemkv::DiscTitle) {
    out.raw(
        Normal,
        &format!("  {}: {}", strings::get("disc.streams"), meta.streams.len()),
    );
    for s in &meta.streams {
        match s {
            libfreemkv::Stream::Video(v) => {
                let label = if v.label.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", v.label)
                };
                out.raw(
                    Normal,
                    &format!("    {} {}{}", v.codec, v.resolution, label),
                );
            }
            libfreemkv::Stream::Audio(a) => {
                let mut tags: Vec<String> = Vec::new();
                if let Some(key) = audio_purpose_key(a.purpose) {
                    tags.push(strings::get(key));
                }
                if a.secondary {
                    tags.push(strings::get("stream.secondary"));
                }
                if !a.label.is_empty() {
                    tags.push(a.label.clone());
                }
                let label = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", tags.join(", "))
                };
                out.raw(
                    Normal,
                    &format!("    {} {} {}{}", a.codec, a.channels, a.language, label),
                );
            }
            libfreemkv::Stream::Subtitle(s) => {
                out.raw(Normal, &format!("    {} {}", s.codec, s.language));
            }
        }
    }
    if meta.duration_secs > 0.0 {
        let d = meta.duration_secs;
        out.raw(
            Normal,
            &format!(
                "  {}: {}:{:02}:{:02}",
                strings::get("disc.duration"),
                d as u64 / 3600,
                (d as u64 % 3600) / 60,
                d as u64 % 60
            ),
        );
    }
}

/// Whether a token is a positional stream URL (`scheme://...`) rather than a
/// flag value. A value-flag (`-t`, `--keydb`) must not swallow one of these.
fn is_url_token(s: &str) -> bool {
    s.contains("://")
}

/// Whether a token is a plausible key-service URL value for `--key-url` — i.e.
/// an `http(s)://` URL. This is the gate that lets `--key-url https://…` accept
/// its value (which `is_url_token` would otherwise treat as a positional stream
/// URL) while still rejecting a missing value (a following flag, or a stream
/// URL with a non-http scheme like `disc://`). The full SSRF/host validation is
/// `freemkv_keysources::validate_keyserver_url`, applied at source-build time.
fn is_keyserver_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// The `Disc::copy` progress callback returns `true` to continue, `false` to
/// halt. Halt the moment SIGINT was seen so the first Ctrl-C stops the copy
/// cleanly (letting the tray unlock on drop) instead of being ignored.
fn copy_should_continue(interrupted: bool) -> bool {
    !interrupted
}

/// Whether a mux must bail instead of finalizing the output. True if SIGINT was
/// seen at any point: either mid-loop (`loop_interrupted`) OR during the final
/// `input.read()` that returned `Ok(None)` and broke the loop without tripping
/// the top-of-loop check (`flag_now` re-reads the global flag right before
/// `output.finish()`). Finalizing after an interrupt would write the container
/// footer over a truncated body and report success on a partial file.
fn mux_was_interrupted(loop_interrupted: bool, flag_now: bool) -> bool {
    loop_interrupted || flag_now
}

/// Whether a 0-based title index is within a source's title count. An explicit
/// out-of-range `-t` on a scanned source is a hard failure (the caller sets
/// `ok = false`), so the CLI exits non-zero instead of reporting success after
/// ripping nothing.
fn title_in_range(idx: usize, count: usize) -> bool {
    idx < count
}

fn sanitize_name(name: &str) -> String {
    let s = name
        .replace(
            |c: char| !c.is_ascii_alphanumeric() && c != ' ' && c != '-' && c != '_',
            "",
        )
        .trim()
        .replace(' ', "_");
    if s.is_empty() { "disc".to_string() } else { s }
}

/// Map `LabelPurpose` to its locale string key. `Normal` → no tag.
fn audio_purpose_key(p: libfreemkv::LabelPurpose) -> Option<&'static str> {
    match p {
        libfreemkv::LabelPurpose::Commentary => Some("stream.purpose.commentary"),
        libfreemkv::LabelPurpose::Descriptive => Some("stream.purpose.descriptive"),
        libfreemkv::LabelPurpose::Score => Some("stream.purpose.score"),
        libfreemkv::LabelPurpose::Ime => Some("stream.purpose.ime"),
        libfreemkv::LabelPurpose::Normal => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KeyConfig, build_jobs, build_key_sources, copy_should_continue, dest_is_iso,
        disc_copy_recovered_data, fmt_disc_damage, fmt_err, fmt_err_str, headers_resolved,
        is_keyserver_url, is_scheme_only_sink, is_title_failure_fatal, is_url_token,
        mux_produced_output, mux_was_interrupted, parse_error_code, parse_flags,
        preflight_validate, render_error, resolved_keydb_path, sanitize_name, title_in_range,
        validate_file_dest, validate_iso_input,
    };
    use crate::output::Output;
    use crate::strings;
    use libfreemkv::parse_url;

    // The decrypt no-key verdict matrix (AACS / CSS / css_error / --raw /
    // unencrypted) now lives in `libfreemkv::Disc::ensure_decryptable[_keys]`,
    // which both CLI entry points (`pipe_disc`, `disc_to_iso`) and the ISO mux
    // (`libfreemkv::input`) delegate to. It is exhaustively tested at that single
    // source of truth in `libfreemkv::disc::tests`, so the CLI no longer carries
    // its own copies of the matrix. The CLI-specific concern — that the resulting
    // `Error::NoDiscKey` renders to an English message with no raw code leak — is
    // covered by `no_keydb_aacs_disc_surfaces_e7022_in_english` below.

    // ── zero-output guard (Theme A fix #1/#2) ───────────────────────────────

    /// The success guard both pipe paths run before `output.finish()` +
    /// "Complete": a drain that wrote no streams OR no frame bytes must be
    /// reported as NOT produced (→ caller errors with NoStreams, nonzero
    /// exit), never finalized as an empty/garbage "success".
    #[test]
    fn mux_produced_output_requires_streams_and_bytes() {
        // Real output: at least one stream AND ≥1 payload byte.
        assert!(mux_produced_output(2, 1));
        assert!(mux_produced_output(1, 5_000_000));
        // Zero streams → never produced (even if some bytes somehow counted).
        assert!(!mux_produced_output(0, 0));
        assert!(!mux_produced_output(0, 1000));
        // Zero bytes written → never produced (the natural-drain-on-first-None
        // empty-output silent failure).
        assert!(!mux_produced_output(3, 0));
    }

    // ── is_title_failure_fatal: skip an incidental copy-protected extra ───────

    /// The E7023 Display string libfreemkv emits for a `CssKeyMissing` per-title
    /// failure (a bare-code error, no trailing data).
    const E7023: &str = "E7023";

    /// (a) Multi-title all-titles rip, a NON-feature title hits E7023: SKIP, not
    /// fatal. The remaining titles still mux and the command can exit 0. This is
    /// the core "one extra protected stub must not kill the whole rip" fix.
    #[test]
    fn title_failure_e7023_non_feature_all_titles_is_skippable() {
        // multi_title=true, explicit_selection=false, is_feature=false.
        assert!(
            !is_title_failure_fatal(E7023, true, false, false),
            "an incidental copy-protected extra title must be skipped, not fatal"
        );
    }

    /// (c) `-t N` on a ScrambledUncracked title — the title the user explicitly
    /// asked for — DOES hard-error, even though it's not the feature.
    #[test]
    fn title_failure_e7023_explicit_selection_is_fatal() {
        // explicit_selection=true → fatal regardless of multi_title / is_feature.
        assert!(is_title_failure_fatal(E7023, true, true, false));
        assert!(is_title_failure_fatal(E7023, false, true, false));
    }

    /// The MAIN FEATURE failing with E7023 is always a hard error — even in an
    /// all-titles rip with no explicit selection. The user wants the movie.
    #[test]
    fn title_failure_e7023_feature_is_fatal() {
        assert!(is_title_failure_fatal(E7023, true, false, true));
    }

    /// A single-title rip (only one job) that hits E7023 is fatal: there is no
    /// "other title" to carry on with, so the lone failure is the result.
    #[test]
    fn title_failure_e7023_single_title_is_fatal() {
        // multi_title=false, not the feature, no explicit selection.
        assert!(is_title_failure_fatal(E7023, false, false, false));
    }

    /// A NON-E7023 error (IO, AACS NoDiscKey/E7022, MkvInvalid, …) is ALWAYS
    /// fatal — only a copy-protection skip (E7023) is ever skippable. A real
    /// problem must never be silently swallowed as "skip the title".
    #[test]
    fn title_failure_non_e7023_is_always_fatal() {
        // Even in the otherwise-skippable shape (multi/non-explicit/non-feature).
        assert!(is_title_failure_fatal(
            "E7022: abcd1234",
            true,
            false,
            false
        ));
        assert!(is_title_failure_fatal("E6000: 12345", true, false, false));
        assert!(is_title_failure_fatal("E8001", true, false, false));
        // A non-code CLI string is also fatal (not a CSS skip).
        assert!(is_title_failure_fatal("some io error", true, false, false));
    }

    /// The skip warning the loop prints (`rip.title_skipped`) exists in en.json
    /// and carries the `{num}` placeholder — so a skipped title surfaces a clear,
    /// localized, non-error message rather than a raw E7023.
    #[test]
    fn rip_title_skipped_string_present_and_localized() {
        let s = strings::fmt("rip.title_skipped", &[("num", "3")]);
        assert_ne!(s, "rip.title_skipped", "missing locale entry");
        assert!(s.contains('3'), "num placeholder not substituted: {s}");
        assert!(
            !s.contains("E7023") && !s.to_lowercase().contains("error"),
            "skip notice must not look like a hard error: {s}"
        );
    }

    /// The disc→ISO sweep-success guard. `Disc::copy` returns `Ok` even when the
    /// whole disc was unreadable (`bytes_good == 0`, every ECC block zero-filled
    /// and marked NonTrimmed) — the ISO is all zeroes and unusable. The guard
    /// must report that as NOT recovered (→ caller prints `rip.no_data`, exits
    /// non-zero), never as a "Complete" success.
    #[test]
    fn disc_copy_recovered_data_gates_zero_recovery() {
        // Whole disc unreadable → no data recovered → not a success.
        assert!(!disc_copy_recovered_data(0));
        // Any recovered bytes → success.
        assert!(disc_copy_recovered_data(1));
        assert!(disc_copy_recovered_data(50_000_000_000));
    }

    /// The header-resolution gate both pipe paths run after their
    /// `while !input.headers_ready()` loop. EOF can break that loop before the
    /// video codec_private (hvcC/avcC) resolves; proceeding would mux a track
    /// header with no CODEC_PRIVATE and still exit 0 (the zero-output guard
    /// passes once any audio byte is written). `headers_resolved(false)` must
    /// be `false` so the caller errors with `MkvInvalid` instead of finalizing
    /// a structurally-invalid MKV.
    #[test]
    fn headers_resolved_rejects_unready_headers() {
        // Headers never became ready (EOF before video codec_private) → abort.
        assert!(!headers_resolved(false));
        // Headers resolved normally → proceed to mux.
        assert!(headers_resolved(true));
    }

    // ── fmt_err generalization (english errors for ALL codes) ───────────────

    /// `parse_error_code` splits the libfreemkv `E<code>[: <data>]` Display
    /// form into the code token and its trailing data.
    #[test]
    fn parse_error_code_splits_code_and_data() {
        assert_eq!(parse_error_code("E6009"), Some(("E6009", "")));
        assert_eq!(parse_error_code("E7022: abcdef"), Some(("E7022", "abcdef")));
        assert_eq!(parse_error_code("E5000: 13"), Some(("E5000", "13")));
        // Not an E-code: returns None (falls through to the generic wrapper).
        assert_eq!(parse_error_code("No drive found"), None);
        assert_eq!(parse_error_code("Error: boom"), None);
        assert_eq!(parse_error_code("E"), None);
        assert_eq!(parse_error_code("Eabc"), None);
    }

    /// A representative sample of codes must render to their ENGLISH locale
    /// strings, prefixed with the language-neutral `E<code>` token (WS2: the
    /// code is SHOWN, not stripped). `fmt_err_str` returns the prefix-free-of-
    /// level `E<code> <message>` fragment; the `Error:` level word is added by
    /// the render site (`render_error`).
    #[test]
    fn fmt_err_renders_codes_to_english() {
        // E6009 NoStreams — the Theme A zero-output error. Code now prefixed,
        // message dejargoned to the user-facing "no audio or video streams".
        let s = fmt_err_str("E6009");
        assert!(s.starts_with("E6009 "), "code not prefixed: {s}");
        assert!(
            s.to_lowercase().contains("no audio or video streams"),
            "got: {s}"
        );

        // E7023 CssKeyMissing — the Theme B CSS gate error. The user-facing
        // copy is dejargoned: "copy-protected", not "CSS title key".
        let s = fmt_err_str("E7023");
        assert!(s.starts_with("E7023 "), "code not prefixed: {s}");
        assert!(s.to_lowercase().contains("copy-protected"), "got: {s}");

        // E9023 MuxEmpty — the Theme A m2ts zero-frame error. Dejargoned to
        // "empty file" / "video or audio", not the internal "mux" term.
        let s = fmt_err_str("E9023");
        assert!(s.starts_with("E9023 "), "code not prefixed: {s}");
        assert!(s.to_lowercase().contains("empty file"), "got: {s}");

        // E5000 with data → {detail} substituted, code prefixed.
        let s = fmt_err_str("E5000: 13");
        assert!(s.starts_with("E5000 "), "code not prefixed: {s}");
        assert!(s.contains("13"), "detail not substituted: {s}");

        // E7013 Decryption failed — code now prefixed.
        let s = fmt_err_str("E7013");
        assert!(s.starts_with("E7013 "), "code not prefixed: {s}");
        assert!(s.to_lowercase().contains("decryption failed"), "got: {s}");

        // E7022 names the disc by hash, code prefixed.
        let s = fmt_err_str("E7022: deadbeef");
        assert!(s.starts_with("E7022 "), "code not prefixed: {s}");
        assert!(s.contains("deadbeef"), "hash not substituted: {s}");
    }

    /// The full render-site output: `render_error` prefixes the `Error:` level
    /// word exactly once onto the `E<code> <message>` fragment (WS2 §2.1).
    #[test]
    fn render_error_prefixes_level_once() {
        let rendered = render_error(&"E6009");
        assert!(rendered.starts_with("Error: E6009 "), "got: {rendered}");
        // The level word appears exactly once (no nested doubling).
        assert_eq!(rendered.matches("Error:").count(), 1);
    }

    /// E6000 (DiscRead) Display is `E6000: <sector> 0x..status../0x..sense..`.
    /// The status/sense hex tail is diagnostic noise that must NOT reach the
    /// user — only the sector number is substituted into the localized message.
    #[test]
    fn fmt_err_e6000_strips_status_sense_hex_tail() {
        // Full DiscRead Display: sector + status + sense triple. The code is
        // now shown as a prefix; the status/sense hex tail is still stripped.
        let s = fmt_err_str("E6000: 7476928 0x02/0x03/0x11/0x00");
        assert!(s.starts_with("E6000 "), "code not prefixed: {s}");
        assert!(s.contains("7476928"), "sector number lost: {s}");
        assert!(!s.contains("0x"), "raw hex tail leaked to user: {s}");
        // Sense-only form (no status byte) also strips the tail.
        let s = fmt_err_str("E6000: 100 0x03/0x11/0x00");
        assert!(s.contains("100") && !s.contains("0x"), "got: {s}");
        // Bare sector (no tail at all) renders cleanly.
        let s = fmt_err_str("E6000: 42");
        assert!(s.contains("42") && !s.contains("0x"), "got: {s}");
    }

    /// A code with NO locale entry falls back to the generic wrapper, which
    /// (WS2) still SHOWS the code via `{code} {detail}` rather than swallowing
    /// it — the last resort, not the common path. The `Error:` level word is
    /// added by the render site, not by `fmt_err_str`.
    #[test]
    fn fmt_err_unknown_code_uses_generic_wrapper() {
        // E1234 has no locale entry; the generic wrapper keeps the code.
        let s = fmt_err_str("E1234: whatever");
        assert_eq!(s, "E1234 whatever");
        // Through the render site the code is still shown with the level word.
        assert_eq!(render_error(&"E1234: whatever"), "Error: E1234 whatever");
    }

    /// A non-code error string (e.g. a CLI-side message) passes through the
    /// generic wrapper with an empty code, so `fmt_err_str` yields the bare
    /// string and the render site prefixes the level word.
    #[test]
    fn fmt_err_non_code_string_uses_generic() {
        // Empty code → leading space trimmed away by the render contract; the
        // fragment carries just the message.
        let s = fmt_err_str("No BD drive found");
        assert!(s.contains("No BD drive found"), "got: {s}");
        assert!(!s.contains('E'), "no spurious code token: {s}");
        assert_eq!(
            render_error(&"No BD drive found"),
            "Error: No BD drive found"
        );
    }

    // ── negative path: no-keydb AACS disc → E7022 surfaced in English ───────

    /// End-to-end negative-path coverage: when the decrypt gate
    /// (`Disc::ensure_decryptable`, tested in libfreemkv) fires for a no-keydb
    /// AACS disc, `pipe_disc`/`disc_to_iso` surface `Error::NoDiscKey`'s Display
    /// (`E7022[: hash]`). This test pins the CLI-side rendering: that string must
    /// render to the ENGLISH E7022 message via `fmt_err` (so the user never sees
    /// a raw `E7022`) and name the disc by hash. The exit-code wiring is
    /// exercised by `run()` returning `false` on any `pipe_disc` Err.
    #[test]
    fn no_keydb_aacs_disc_surfaces_e7022_in_english() {
        // The error pipe_disc returns, rendered for the user.
        let disp = libfreemkv::Error::NoDiscKey {
            disc_hash: "deadbeefcafe".to_string(),
        }
        .to_string();
        assert!(
            disp.starts_with("E7022"),
            "library Display is E7022: {disp}"
        );
        let rendered = fmt_err_str(&disp);
        // English, names the disc by hash, code SHOWN (WS2: code-forward).
        assert!(rendered.contains("deadbeefcafe"), "hash named: {rendered}");
        assert!(
            rendered.starts_with("E7022 "),
            "code not prefixed: {rendered}"
        );
        assert!(
            rendered.to_lowercase().contains("key"),
            "english key message: {rendered}"
        );
    }

    #[test]
    fn copy_halts_on_first_interrupt() {
        // The Ctrl-C fix: the copy progress callback must return false (halt) the
        // moment SIGINT is seen, so the first Ctrl-C stops the sweep and the
        // tray unlocks on drop — rather than being ignored until `_exit(130)`.
        assert!(copy_should_continue(false), "no interrupt → keep going");
        assert!(!copy_should_continue(true), "interrupt → halt the copy");
    }

    #[test]
    fn mux_bails_when_interrupt_arrives_during_final_read() {
        // The window: a SIGINT during the final `input.read()` (the one that
        // returns `Ok(None)`) breaks the loop WITHOUT setting `loop_interrupted`,
        // so the pre-`finish()` re-read of the global flag is what catches it.
        assert!(
            !mux_was_interrupted(false, false),
            "clean finish → finalize"
        );
        assert!(mux_was_interrupted(true, false), "mid-loop SIGINT → bail");
        assert!(
            mux_was_interrupted(false, true),
            "SIGINT during the final read (flag set, loop flag stale) → still bail"
        );
        assert!(mux_was_interrupted(true, true), "both → bail");
    }

    #[test]
    fn work_pct_is_finite_when_work_total_zero() {
        // `print_disc_progress` now derives `pct` from `PassProgress::work_pct()`,
        // which guards `work_total == 0` (returns 100.0). The old inline
        // `work_done / work_total` produced `NaN%` for an empty Sweep/Mux pass.
        let p = libfreemkv::progress::PassProgress {
            kind: libfreemkv::progress::PassKind::Sweep,
            work_done: 0,
            work_total: 0,
            bytes_good_total: 0,
            bytes_unreadable_total: 0,
            bytes_pending_total: 0,
            bytes_retryable_total: 0,
            bytes_total_disc: 0,
            disc_duration_secs: None,
            bytes_bad_in_main_title: 0,
            main_title_duration_secs: None,
            main_title_size_bytes: None,
            located: Default::default(),
        };
        let pct = p.work_pct();
        assert!(pct.is_finite(), "work_total==0 must not yield NaN%");
        assert_eq!(pct, 100.0);
    }

    /// A healthy in-progress rip (zero read errors, large unread remainder)
    /// must render the clean "no loss" damage string, NOT a "lost" string.
    /// Regression: `print_disc_progress` used to fold `bytes_pending_total`
    /// (which includes not-yet-read NonTried sectors) into the "lost" total,
    /// so an 8%-done rip displayed ~92% of runtime as "lost (in movie)".
    #[test]
    fn disc_damage_unread_is_not_lost() {
        let p = libfreemkv::progress::PassProgress {
            kind: libfreemkv::progress::PassKind::Sweep,
            work_done: 800,
            work_total: 10_000,
            bytes_good_total: 800,
            bytes_unreadable_total: 0,
            // 92% of the disc not yet read — large pending, but ZERO failed.
            bytes_pending_total: 9_200,
            bytes_retryable_total: 0,
            bytes_total_disc: 10_000,
            disc_duration_secs: Some(7200.0),
            bytes_bad_in_main_title: 0,
            main_title_duration_secs: Some(7200.0),
            main_title_size_bytes: Some(10_000),
            located: Default::default(),
        };
        let damage = fmt_disc_damage(&p);
        assert_eq!(
            damage,
            strings::get("rip.damage_none"),
            "unread sectors must not count as lost; got {damage:?}"
        );
        assert!(
            !damage.contains("lost"),
            "healthy rip must not render a 'lost' string; got {damage:?}"
        );
    }

    /// Sectors that actually FAILED to read (unreadable, or retryable =
    /// NonTrimmed/NonScraped awaiting retry) DO count as lost.
    #[test]
    fn disc_damage_failed_reads_are_lost() {
        // Retryable (failed-awaiting-retry) alone triggers "lost".
        let p_retryable = libfreemkv::progress::PassProgress {
            kind: libfreemkv::progress::PassKind::Sweep,
            work_done: 5_000,
            work_total: 10_000,
            bytes_good_total: 4_900,
            bytes_unreadable_total: 0,
            bytes_pending_total: 5_100,
            bytes_retryable_total: 100,
            bytes_total_disc: 10_000,
            disc_duration_secs: Some(7200.0),
            bytes_bad_in_main_title: 0,
            main_title_duration_secs: Some(7200.0),
            main_title_size_bytes: Some(10_000),
            located: Default::default(),
        };
        let damage = fmt_disc_damage(&p_retryable);
        assert!(
            damage.contains("lost"),
            "failed-awaiting-retry must render a 'lost' string; got {damage:?}"
        );
        assert_ne!(damage, strings::get("rip.damage_none"));

        // Unreadable (gave up) alone also triggers "lost".
        let p_unreadable = libfreemkv::progress::PassProgress {
            bytes_unreadable_total: 100,
            bytes_retryable_total: 0,
            ..p_retryable
        };
        assert!(
            fmt_disc_damage(&p_unreadable).contains("lost"),
            "unreadable bytes must render a 'lost' string"
        );
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stream_info_uses_dedicated_keys() {
        // Regression: `print_stream_info` mislabeled the elementary-track count
        // with `disc.titles` ("Titles: 7") and the runtime with `disc.format`
        // ("Format: 2:34:10"). Both now have dedicated keys that must resolve to
        // real strings — `strings::get` returns the dotted path verbatim on a
        // miss, so a present key is one that does NOT equal its own path.
        assert_ne!(crate::strings::get("disc.streams"), "disc.streams");
        assert_ne!(crate::strings::get("disc.duration"), "disc.duration");
        // And they must be distinct from the keys they were confused with, so a
        // future copy-paste can't silently re-alias them.
        assert_ne!(
            crate::strings::get("disc.streams"),
            crate::strings::get("disc.titles")
        );
        assert_ne!(
            crate::strings::get("disc.duration"),
            crate::strings::get("disc.format")
        );
    }

    #[test]
    fn url_token_detection() {
        assert!(is_url_token("disc://"));
        assert!(is_url_token("mkv://out.mkv"));
        assert!(!is_url_token("1"));
        assert!(!is_url_token("keydb.cfg"));
        assert!(!is_url_token("/path/out.mkv"));
    }

    #[test]
    fn title_one_based_value_accepted() {
        let f = parse_flags(&v(&["-t", "1", "-t", "3"])).unwrap();
        assert_eq!(f.title_nums, vec![1, 3]);
    }

    #[test]
    fn duplicate_title_flags_dedup() {
        // `-t 1 -t 1` must collapse to a single title, not two jobs that both
        // map to the same index and overwrite the same output file.
        let f = parse_flags(&v(&["-t", "1", "-t", "1"])).unwrap();
        assert_eq!(f.title_nums, vec![1]);
        // Out-of-order repeats sort + dedup deterministically.
        let f = parse_flags(&v(&["-t", "3", "-t", "1", "-t", "3"])).unwrap();
        assert_eq!(f.title_nums, vec![1, 3]);
    }

    #[test]
    fn disc_multiple_titles_build_one_job_each() {
        // Regression (HIGH): multiple `-t` on a disc source must build one job
        // per requested title — not silently drop all but the first. `titles`
        // is None for a disc (pipe_disc scans per title); the jobs come straight
        // from title_nums.
        let out = Output::new(false, true);
        // Repo-local scratch (not /tmp): survives reboots and stays inside the
        // build tree so stray dirs are obvious and cleaned by `cargo clean`.
        let dest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!("freemkv_test_{}", std::process::id()));
        let dest = format!("mkv://{}", dest_dir.display());
        let parsed_dest = libfreemkv::parse_url(&dest);

        let jobs = build_jobs(
            &None,
            true, // is_disc
            &[1usize, 3usize],
            true, // is_dir_dest — multiple titles require a directory dest
            &dest,
            &parsed_dest,
            &out,
        )
        .expect("dir creation should succeed in temp");

        assert_eq!(jobs.len(), 2, "both -t 1 and -t 3 must produce a job");
        // Title indices are 0-based: -t 1 → 0, -t 3 → 2.
        assert_eq!(jobs[0].0, Some(0));
        assert_eq!(jobs[1].0, Some(2));
        // Distinct output files (no silent overwrite / drop).
        assert_ne!(jobs[0].1, jobs[1].1);
        assert!(jobs[0].1.contains("_t1."), "got {}", jobs[0].1);
        assert!(jobs[1].1.contains("_t3."), "got {}", jobs[1].1);

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    #[test]
    fn disc_multiple_titles_to_file_dest_rejected() {
        // Regression (MEDIUM): a disc multi-title rip to a single-FILE dest used
        // to fall into dir_jobs, which `create_dir_all`s the dest — silently
        // turning `movie.mkv` into a directory. It must now be rejected (build
        // returns None) when the dest is not directory-style, mirroring the
        // scanned-source guard.
        let out = Output::new(false, true);
        let parsed_dest = libfreemkv::parse_url("mkv://movie.mkv");
        let jobs = build_jobs(
            &None,
            true, // is_disc
            &[1usize, 2usize],
            false, // is_dir_dest — a single file can't hold two titles
            "mkv://movie.mkv",
            &parsed_dest,
            &out,
        );
        assert!(
            jobs.is_none(),
            "multi-title disc to a file dest must be rejected, not silently turned into a dir"
        );
        // The file `movie.mkv` must NOT have been created as a directory.
        assert!(
            !std::path::Path::new("movie.mkv").is_dir(),
            "must not have created a directory at the file dest"
        );
    }

    #[test]
    fn out_of_range_title_is_failure() {
        // Regression (HIGH): an explicit `-t` past the last title must be a hard
        // failure (caller sets ok=false → non-zero exit), not a warning that
        // still exits 0. title_in_range gates that branch.
        assert!(title_in_range(0, 3), "first title is in range");
        assert!(title_in_range(2, 3), "last title is in range");
        assert!(!title_in_range(3, 3), "one past the end is out of range");
        assert!(!title_in_range(99, 3), "far past the end is out of range");
        assert!(!title_in_range(0, 0), "no titles → any index out of range");
    }

    #[test]
    fn disc_single_title_is_single_file_job() {
        // A single `-t` on a disc keeps the one-file path (no directory).
        let out = Output::new(false, true);
        let parsed_dest = libfreemkv::parse_url("mkv://out.mkv");
        let jobs = build_jobs(
            &None,
            true,
            &[2usize],
            false,
            "mkv://out.mkv",
            &parsed_dest,
            &out,
        )
        .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, Some(1));
        assert_eq!(jobs[0].1, "mkv://out.mkv");
    }

    #[test]
    fn title_zero_rejected() {
        // `-t 0` must not underflow to all-titles; it's an explicit error.
        let err = parse_flags(&v(&["-t", "0"])).unwrap_err();
        assert!(err.contains('0'), "got: {err}");
    }

    #[test]
    fn title_non_numeric_rejected() {
        // A bad value must NOT silently leave title_nums empty (= all titles).
        let err = parse_flags(&v(&["-t", "main"])).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn title_missing_value_rejected() {
        assert!(parse_flags(&v(&["-t"])).is_err());
        // Followed by a URL → value is missing, not the URL.
        assert!(parse_flags(&v(&["-t", "disc://"])).is_err());
    }

    #[test]
    fn keydb_missing_value_rejected() {
        // `--keydb` with no value must not silently fall back to the default keydb.
        assert!(parse_flags(&v(&["--keydb"])).is_err());
        assert!(parse_flags(&v(&["--keydb", "disc://"])).is_err());
    }

    #[test]
    fn keydb_value_accepted() {
        let f = parse_flags(&v(&["--keydb", "/etc/keydb.cfg"])).unwrap();
        assert_eq!(f.keydb_path.as_deref(), Some("/etc/keydb.cfg"));
    }

    // ── Online key-source flags ────────────────────────────────────────────

    #[test]
    fn is_keyserver_url_accepts_http_only() {
        assert!(is_keyserver_url("http://keys.example/keys"));
        assert!(is_keyserver_url("https://keys.example/keys"));
        // A stream URL with a non-http scheme is NOT a key-service URL value.
        assert!(!is_keyserver_url("disc://"));
        assert!(!is_keyserver_url("mkv://out.mkv"));
        assert!(!is_keyserver_url("ftp://x/keys"));
        assert!(!is_keyserver_url("--quiet"));
    }

    #[test]
    fn key_url_and_auth_parse() {
        let f = parse_flags(&v(&[
            "--key-url",
            "https://keys.example/keys",
            "--key-auth",
            "tok123",
        ]))
        .unwrap();
        assert_eq!(f.key_url.as_deref(), Some("https://keys.example/keys"));
        assert_eq!(f.key_auth.as_deref(), Some("tok123"));
    }

    #[test]
    fn key_url_missing_or_non_http_value_rejected() {
        // No value at all.
        assert!(parse_flags(&v(&["--key-url"])).is_err());
        // A following stream URL with a non-http scheme is NOT the value —
        // value is missing (must not eat the positional `disc://`).
        assert!(parse_flags(&v(&["--key-url", "disc://"])).is_err());
        // A following flag means the value is missing.
        assert!(parse_flags(&v(&["--key-url", "--quiet"])).is_err());
    }

    // ── VAL-2 regression: --key-url scheme validation ──────────────────────
    //
    // Bug: the guard was `!is_url_token(u)` (i.e. `!u.contains("://")`) so the
    // bad-scheme branch's `u.contains("://") && !is_keyserver_url(u)` was
    // `A && !A` — dead code that could never fire. `ftp://x` and `disc://` both
    // fell through to "requires a value" even though the user DID supply a value.
    // Fix: guard the accept arm on `is_keyserver_url(u)` so the bad-scheme arm
    // is reachable for any `://` URL that is NOT http(s).

    /// VAL-2: `--key-url ftp://x` — a non-http(s) scheme — must produce the
    /// bad-scheme error, NOT "requires a value" (the value was present).
    #[test]
    fn val2_key_url_ftp_scheme_gives_bad_scheme_error() {
        let err = parse_flags(&v(&["--key-url", "ftp://x"])).unwrap_err();
        // Must contain the bad-scheme message substring, not the generic
        // "requires a value" substring.
        assert!(
            err.contains("http://") || err.contains("https://"),
            "expected bad-scheme error (mentioning http(s)://), got: {err}"
        );
        assert!(
            !err.contains("requires a value"),
            "must NOT produce flag_needs_value when a value was present: {err}"
        );
        // The bad URL itself must appear in the message so the user can see
        // what was rejected.
        assert!(
            err.contains("ftp://x"),
            "rejected URL missing from error: {err}"
        );
    }

    /// VAL-2: `--key-url disc://` — a stream scheme used as a key-url — must
    /// also produce the bad-scheme error. `disc://` contains `://` but is not
    /// http(s), so it goes through the bad-scheme arm, not the missing-value arm.
    #[test]
    fn val2_key_url_disc_scheme_gives_bad_scheme_error() {
        let err = parse_flags(&v(&["--key-url", "disc://"])).unwrap_err();
        assert!(
            err.contains("http://") || err.contains("https://"),
            "expected bad-scheme error (mentioning http(s)://), got: {err}"
        );
        assert!(
            !err.contains("requires a value"),
            "must NOT produce flag_needs_value when a value (with wrong scheme) was present: {err}"
        );
        assert!(
            err.contains("disc://"),
            "rejected URL missing from error: {err}"
        );
    }

    /// VAL-2 (positive path): `--key-url https://keys.example/keys` must be
    /// accepted and stored verbatim.
    #[test]
    fn val2_key_url_https_accepted() {
        let f = parse_flags(&v(&["--key-url", "https://keys.example/keys"])).unwrap();
        assert_eq!(
            f.key_url.as_deref(),
            Some("https://keys.example/keys"),
            "https key-url must be accepted and stored verbatim"
        );
    }

    /// VAL-2 (positive path): `--key-url http://keys.example/keys` (plain http)
    /// must also be accepted.
    #[test]
    fn val2_key_url_http_accepted() {
        let f = parse_flags(&v(&["--key-url", "http://keys.example/keys"])).unwrap();
        assert_eq!(
            f.key_url.as_deref(),
            Some("http://keys.example/keys"),
            "http key-url must be accepted and stored verbatim"
        );
    }

    /// VAL-2 (missing value): bare `--key-url` with no following token must
    /// produce the flag_needs_value error (not the bad-scheme error).
    #[test]
    fn val2_key_url_no_value_gives_needs_value_error() {
        let err = parse_flags(&v(&["--key-url"])).unwrap_err();
        assert!(
            err.contains("requires a value"),
            "bare --key-url must produce flag_needs_value, got: {err}"
        );
    }

    /// VAL-2 (missing value via flag): `--key-url --quiet` — the value is a
    /// flag, not a URL, so it is missing. Must produce flag_needs_value.
    #[test]
    fn val2_key_url_followed_by_flag_gives_needs_value_error() {
        let err = parse_flags(&v(&["--key-url", "--quiet"])).unwrap_err();
        assert!(
            err.contains("requires a value"),
            "--key-url followed by a flag must produce flag_needs_value, got: {err}"
        );
    }

    #[test]
    fn key_auth_missing_value_rejected() {
        assert!(parse_flags(&v(&["--key-auth"])).is_err());
        // A following stream URL means the token was omitted.
        assert!(parse_flags(&v(&["--key-auth", "disc://"])).is_err());
    }

    /// Source assembly per the agreed design — local-first ordering, pinned via
    /// each source's stable `label()` (`"keydb"` before `"online"`).
    #[test]
    fn build_key_sources_orders_local_first() {
        let out = Output::new(false, true);

        // keydb only → [Keydb]. (Default location is fine; we only inspect order.)
        let s = build_key_sources(
            &KeyConfig {
                keydb_path: Some("keydb.cfg".into()),
                key_url: None,
                key_auth: None,
            },
            &out,
        );
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].label(),
            "keydb",
            "keydb-only first source is the keydb"
        );

        // neither flag → still [Keydb] (default keydb location).
        let s = build_key_sources(&KeyConfig::default(), &out);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label(), "keydb", "no flags → keydb only");

        // --key-url only → [Online] (no keydb consulted).
        let s = build_key_sources(
            &KeyConfig {
                keydb_path: None,
                key_url: Some("https://8.8.8.8/keys".into()),
                key_auth: None,
            },
            &out,
        );
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].label(),
            "online",
            "url-only first source is the online one"
        );

        // both → [Keydb, Online] — LOCAL-FIRST.
        let s = build_key_sources(
            &KeyConfig {
                keydb_path: Some("keydb.cfg".into()),
                key_url: Some("https://8.8.8.8/keys".into()),
                key_auth: Some("tok".into()),
            },
            &out,
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].label(), "keydb", "local keydb is tried first");
        assert_eq!(s[1].label(), "online", "online service is the fallback");
    }

    /// SSRF guard: a `--key-url` that resolves to an internal / metadata host is
    /// dropped (not added as a source) — `build_key_sources` does not POST key
    /// material there. With keydb present, the keydb remains; url-only yields no
    /// sources at all.
    #[test]
    fn build_key_sources_drops_ssrf_rejected_url() {
        let out = Output::new(false, true);

        // url-only, metadata endpoint → rejected → zero sources.
        let s = build_key_sources(
            &KeyConfig {
                keydb_path: None,
                key_url: Some("http://169.254.169.254/latest/meta-data".into()),
                key_auth: None,
            },
            &out,
        );
        assert!(
            s.is_empty(),
            "SSRF-rejected url-only must add no online source"
        );

        // url-only, loopback → rejected → zero sources.
        let s = build_key_sources(
            &KeyConfig {
                keydb_path: None,
                key_url: Some("https://127.0.0.1:8443/keys".into()),
                key_auth: None,
            },
            &out,
        );
        assert!(s.is_empty(), "loopback url must be rejected");

        // keydb + rejected url → only the keydb survives.
        let s = build_key_sources(
            &KeyConfig {
                keydb_path: Some("keydb.cfg".into()),
                key_url: Some(format!("http://{}.{}.{}.{}/keys", 10, 0, 0, 5)),
                key_auth: None,
            },
            &out,
        );
        assert_eq!(s.len(), 1, "rejected url dropped; keydb remains");
        assert_eq!(s[0].label(), "keydb", "the surviving source is the keydb");
    }

    #[test]
    fn unknown_flag_is_rejected() {
        // Regression (MEDIUM): a typo'd flag (`--titel`, `--qiet`) used to fall
        // through the catch-all and be silently ignored — defaults used, exit 0.
        // It must now be a hard error.
        assert!(parse_flags(&v(&["--titel", "1"])).is_err());
        assert!(parse_flags(&v(&["--qiet"])).is_err());
        assert!(parse_flags(&v(&["-x"])).is_err());
        // The error names the offending flag.
        let err = parse_flags(&v(&["--bogus"])).unwrap_err();
        assert!(err.contains("--bogus"), "got: {err}");
        // Non-dash positionals (URLs, title values) are NOT rejected here.
        assert!(parse_flags(&v(&["disc://", "mkv://out.mkv"])).is_ok());
        assert!(parse_flags(&v(&["-t", "1", "disc://"])).is_ok());
    }

    #[test]
    fn boolean_flags_parse() {
        // `--log-level 2` (info) widens prose detail → verbose.
        let f = parse_flags(&v(&["--raw", "--multipass", "--log-level", "2", "-q"])).unwrap();
        assert!(f.raw && f.multipass && f.verbose && f.quiet);
        assert!(f.title_nums.is_empty());
        assert!(!f.force, "force defaults off");
    }

    #[test]
    fn force_flag_parses() {
        // `--force` opts into overwriting a non-empty dir:// target.
        let f = parse_flags(&v(&["--force"])).unwrap();
        assert!(f.force);
        assert!(!parse_flags(&v(&[])).unwrap().force);
    }

    #[test]
    fn log_level_sets_verbose_at_or_above_two() {
        // Level 1 = quiet prose; 2/3/4 widen it. The numeric value must also be
        // consumed so it is never mistaken for a positional URL.
        assert!(!parse_flags(&v(&["--log-level", "1"])).unwrap().verbose);
        assert!(parse_flags(&v(&["--log-level", "2"])).unwrap().verbose);
        assert!(parse_flags(&v(&["--log-level", "4"])).unwrap().verbose);
    }

    #[test]
    fn schemeless_dest_is_unknown() {
        // Backs the `run()` guard that rejects a schemeless dest up front
        // instead of producing `name_t1.unknown` / `unknown://` outputs.
        assert!(matches!(
            libfreemkv::parse_url("out.mkv"),
            libfreemkv::StreamUrl::Unknown { .. }
        ));
        assert!(matches!(
            libfreemkv::parse_url("/path/out.mkv"),
            libfreemkv::StreamUrl::Unknown { .. }
        ));
        assert!(matches!(
            libfreemkv::parse_url("mkv://out.mkv"),
            libfreemkv::StreamUrl::Mkv { .. }
        ));
    }

    // ════════════════════════════════════════════════════════════════════════
    // Adversarial input battery — "tests galore to try and break it".
    //
    // Every bad-input class + combinations, each asserting fail-LOUD-EARLY:
    // `preflight_validate` returns Err (a printable message) — never panics,
    // never silently succeeds. The CLI maps that Err to a printed message +
    // `run()` returning false → nonzero exit + no output.
    // ════════════════════════════════════════════════════════════════════════

    /// Run `preflight_validate` on a (source, dest, raw, multipass) tuple,
    /// parsing the URLs the same way `run()` does. Returns the Result so tests
    /// can assert Ok / Err without repeating the parse boilerplate.
    fn preflight(source: &str, dest: &str, raw: bool, multipass: bool) -> Result<(), String> {
        preflight_f(source, dest, raw, multipass, false)
    }

    /// `preflight` with an explicit `--force` value (for `dir://` non-empty
    /// target tests).
    fn preflight_f(
        source: &str,
        dest: &str,
        raw: bool,
        multipass: bool,
        force: bool,
    ) -> Result<(), String> {
        let ps = parse_url(source);
        let pd = parse_url(dest);
        preflight_validate(source, dest, &ps, &pd, raw, multipass, force)
    }

    /// A unique temp path under the system temp dir (no tempfile dep). Caller
    /// is responsible for cleanup; non-existent by construction.
    fn temp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("freemkv_test_{}_{}_{}", tag, std::process::id(), n))
    }

    // ── schemes ─────────────────────────────────────────────────────────────

    #[test]
    fn preflight_rejects_schemeless_dest() {
        let e = preflight("iso://in.iso", "out.mkv", false, false).unwrap_err();
        assert!(
            e.contains("scheme"),
            "must guide on missing dest scheme: {e}"
        );
    }

    #[test]
    fn preflight_rejects_schemeless_source() {
        // A real readable ISO dest is irrelevant — the schemeless SOURCE must be
        // caught first. Use a sink dest so dest validation can't mask it.
        let e = preflight("in.iso", "null://", false, false).unwrap_err();
        assert!(
            e.to_lowercase().contains("scheme"),
            "must guide on missing source scheme: {e}"
        );
    }

    #[test]
    fn preflight_rejects_unknown_dest_scheme() {
        // `gopher://x` parses to Unknown (no recognized scheme) → rejected.
        let e = preflight("null://", "gopher://x", false, false).unwrap_err();
        assert!(!e.is_empty());
    }

    // ── --raw / --multipass are iso://-only ─────────────────────────────────

    #[test]
    fn raw_rejected_on_mkv_dest() {
        let e = preflight("disc://", "mkv://out.mkv", true, false).unwrap_err();
        assert!(e.contains("--raw"), "names the offending flag: {e}");
        assert!(e.contains("iso://"), "points at the supported output: {e}");
    }

    #[test]
    fn raw_rejected_on_m2ts_and_null_and_stdio() {
        for dest in ["m2ts://o.m2ts", "null://", "stdio://"] {
            let e = preflight("disc://", dest, true, false)
                .expect_err(&format!("--raw on {dest} must error"));
            assert!(e.contains("--raw"), "{dest}: {e}");
        }
    }

    #[test]
    fn multipass_rejected_on_mkv_dest() {
        let e = preflight("disc://", "mkv://out.mkv", false, true).unwrap_err();
        assert!(e.contains("--multipass"), "names the flag: {e}");
        assert!(e.contains("iso://"), "points at iso://: {e}");
    }

    #[test]
    fn multipass_rejected_on_null_and_stdio_and_network() {
        for dest in ["null://", "stdio://", "network://host:9000"] {
            let e = preflight("disc://", dest, false, true)
                .expect_err(&format!("--multipass on {dest} must error"));
            assert!(e.contains("--multipass"), "{dest}: {e}");
        }
    }

    #[test]
    fn disc_to_mkv_raw_combination_errors() {
        // disc→mkv --raw: the explicit combination called out in the brief.
        assert!(preflight("disc://", "mkv://o.mkv", true, false).is_err());
    }

    #[test]
    fn disc_to_mkv_multipass_combination_errors() {
        assert!(preflight("disc://", "mkv://o.mkv", false, true).is_err());
    }

    #[test]
    fn disc_to_null_raw_and_multipass_error() {
        // disc→null --raw and disc→null --multipass: both error (iso://-only).
        assert!(preflight("disc://", "null://", true, false).is_err());
        assert!(preflight("disc://", "null://", false, true).is_err());
    }

    // ── dir:// (decrypted file-tree extraction) gates ───────────────────────

    /// `--raw` into a `dir://` dest is rejected (dir:// is not iso://, so the
    /// system-wide raw/iso-only gate fires). An encrypted file tree is useless.
    #[test]
    fn dir_dest_rejects_raw() {
        let out = temp_path("dir_raw");
        let dest = format!("dir://{}/", out.display());
        let e = preflight("disc://", &dest, true, false).expect_err("dir:// + --raw must error");
        assert!(e.contains("--raw"), "names the offending flag: {e}");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// `--multipass` into a `dir://` dest is rejected (dir:// is 1-shot;
    /// recovery is the iso:// multipass path's job).
    #[test]
    fn dir_dest_rejects_multipass() {
        let out = temp_path("dir_mp");
        let dest = format!("dir://{}/", out.display());
        let e =
            preflight("disc://", &dest, false, true).expect_err("dir:// + --multipass must error");
        assert!(e.contains("--multipass"), "names the flag: {e}");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A byte-stream source (no filesystem) into `dir://` is rejected up front
    /// — only disc:// / iso:// supply a UDF tree.
    #[test]
    fn dir_dest_rejects_byte_stream_source() {
        for src in [
            "mkv://in.mkv",
            "m2ts://in.m2ts",
            "network://host:9000",
            "stdio://",
        ] {
            let out = temp_path("dir_src");
            let dest = format!("dir://{}/", out.display());
            let e = preflight(src, &dest, false, false)
                .expect_err(&format!("{src} → dir:// should error"));
            assert!(
                e.to_lowercase().contains("dir://") || e.contains("file tree"),
                "{src}: {e}"
            );
            let _ = std::fs::remove_dir_all(&out);
        }
    }

    /// disc:// and iso:// SOURCES into dir:// pass the source gate (an iso://
    /// input still needs a readable file, supplied here).
    #[test]
    fn dir_dest_accepts_disc_and_iso_sources() {
        let out = temp_path("dir_ok");
        let dest = format!("dir://{}/", out.display());
        // disc:// (auto-detect device): source gate passes; dir target created.
        assert!(preflight("disc://", &dest, false, false).is_ok());
        let _ = std::fs::remove_dir_all(&out);

        // iso:// source needs a real, non-empty file.
        let iso = temp_path("dir_ok_iso");
        std::fs::write(&iso, b"not empty").unwrap();
        let out2 = temp_path("dir_ok2");
        let dest2 = format!("dir://{}/", out2.display());
        let src = format!("iso://{}", iso.display());
        assert!(preflight(&src, &dest2, false, false).is_ok());
        let _ = std::fs::remove_file(&iso);
        let _ = std::fs::remove_dir_all(&out2);
    }

    /// A non-empty `dir://` target is refused without `--force`, accepted with.
    #[test]
    fn dir_dest_non_empty_requires_force() {
        let out = temp_path("dir_nonempty");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("x.txt"), b"x").unwrap();
        let dest = format!("dir://{}/", out.display());
        let e =
            preflight_f("disc://", &dest, false, false, false).expect_err("non-empty must error");
        assert!(e.to_lowercase().contains("empty"), "{e}");
        // --force overrides.
        assert!(preflight_f("disc://", &dest, false, false, true).is_ok());
        let _ = std::fs::remove_dir_all(&out);
    }

    /// disc:// → dir:// where the dir:// target is an EXISTING REGULAR FILE is
    /// rejected, and `--force` does NOT override it (force only opts into a
    /// non-empty *directory*; it cannot turn a file into a folder). This pins
    /// the `validate_dir_dest` file-branch on the disc-source path, which the
    /// other dir:// gating tests (raw/multipass/byte-stream/non-empty) leave
    /// uncovered.
    #[test]
    fn dir_dest_existing_file_rejected_even_with_force() {
        let f = temp_path("dir_isfile");
        std::fs::write(&f, b"i am a file, not a folder").unwrap();
        let dest = format!("dir://{}", f.display());

        // Without --force.
        let e = preflight("disc://", &dest, false, false)
            .expect_err("dir:// target that is a file must error");
        assert!(
            e.to_lowercase().contains("file") || e.to_lowercase().contains("folder"),
            "must explain the file/folder mismatch: {e}"
        );

        // --force must NOT rescue it — a regular file is still not a folder.
        let e2 = preflight_f("disc://", &dest, false, false, true)
            .expect_err("dir:// target that is a file must error even with --force");
        assert!(
            e2.to_lowercase().contains("file") || e2.to_lowercase().contains("folder"),
            "--force must not turn a file into a dir:// target: {e2}"
        );
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn raw_and_multipass_accepted_on_iso_dest() {
        // The legit case: iso:// destination accepts both flags. (Source is the
        // live drive, not pre-checked for existence here — device None.)
        assert!(preflight("disc://", "iso://disc.iso", true, false).is_ok());
        assert!(preflight("disc://", "iso://disc.iso", false, true).is_ok());
        assert!(preflight("disc://", "iso://disc.iso", true, true).is_ok());
    }

    #[test]
    fn no_flags_accepted_on_non_iso_dest() {
        // Without the iso-only flags, a mux/sink dest is fine at preflight.
        assert!(preflight("disc://", "mkv://o.mkv", false, false).is_ok());
        assert!(preflight("disc://", "null://", false, false).is_ok());
    }

    // ── drive / device source ────────────────────────────────────────────────

    #[test]
    fn missing_device_path_errors_early() {
        // An explicit device path that doesn't exist must be caught before any
        // open. Use a sink dest so only the source check can fire.
        let e = preflight("disc:///dev/does-not-exist-xyz", "null://", false, false).unwrap_err();
        assert!(
            e.to_lowercase().contains("device") || e.contains("does-not-exist"),
            "must name the missing device: {e}"
        );
    }

    #[test]
    fn auto_detect_device_not_prechecked() {
        // `disc://` with no device path is auto-detect — left to find_drive, so
        // preflight must NOT error on it for source reachability.
        assert!(preflight("disc://", "null://", false, false).is_ok());
    }

    // ── ISO input ────────────────────────────────────────────────────────────

    #[test]
    fn iso_input_missing_errors() {
        let p = temp_path("nope.iso");
        let e = validate_iso_input(&p).unwrap_err();
        assert!(e.to_lowercase().contains("not found"), "{e}");
    }

    #[test]
    fn iso_input_directory_errors() {
        let dir = temp_path("isodir");
        std::fs::create_dir(&dir).unwrap();
        let e = validate_iso_input(&dir).unwrap_err();
        let _ = std::fs::remove_dir(&dir);
        assert!(e.to_lowercase().contains("directory"), "{e}");
    }

    #[test]
    fn iso_input_empty_errors() {
        let f = temp_path("empty.iso");
        std::fs::write(&f, b"").unwrap();
        let e = validate_iso_input(&f).unwrap_err();
        let _ = std::fs::remove_file(&f);
        assert!(e.to_lowercase().contains("empty"), "{e}");
    }

    #[test]
    fn iso_input_nonempty_file_passes_cheap_check() {
        // A non-empty readable file passes the CHEAP preflight (deep image
        // validity is the scan's job, not preflight's).
        let f = temp_path("ok.iso");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let r = validate_iso_input(&f);
        let _ = std::fs::remove_file(&f);
        assert!(r.is_ok(), "non-empty file must pass cheap iso check: {r:?}");
    }

    #[test]
    fn iso_source_missing_errors_through_preflight() {
        // Full path: an iso:// source pointing at a missing file errors in
        // preflight (not just the unit helper).
        let p = temp_path("missing.iso");
        let src = format!("iso://{}", p.display());
        let e = preflight(&src, "null://", false, false).unwrap_err();
        assert!(e.to_lowercase().contains("not found"), "{e}");
    }

    // ── output destination ───────────────────────────────────────────────────

    #[test]
    fn dest_parent_missing_errors() {
        // mkv:// whose parent directory does not exist must error before work.
        let missing_dir = temp_path("no_such_dir");
        let dest = missing_dir.join("movie.mkv");
        let e = validate_file_dest(&dest).unwrap_err();
        assert!(
            e.to_lowercase().contains("director") || e.to_lowercase().contains("exist"),
            "{e}"
        );
    }

    #[test]
    fn dest_is_existing_directory_errors() {
        // A path that is an existing DIRECTORY can't receive a single-file write.
        let dir = temp_path("existing_dir");
        std::fs::create_dir(&dir).unwrap();
        let e = validate_file_dest(&dir).unwrap_err();
        let _ = std::fs::remove_dir(&dir);
        assert!(e.to_lowercase().contains("director"), "{e}");
    }

    #[test]
    fn dest_writable_parent_passes_and_leaves_no_probe_file() {
        // A writable parent + non-existent target passes, and the writability
        // probe must NOT leave its temp file behind.
        let f = temp_path("writable.mkv");
        let r = validate_file_dest(&f);
        assert!(r.is_ok(), "writable dest must pass: {r:?}");
        assert!(
            !f.exists(),
            "the writability probe must clean up its temp file"
        );
    }

    #[test]
    fn dest_writable_check_does_not_truncate_existing_file() {
        // If the target already exists, the probe must NOT truncate it (we open
        // append, not create-new). Pre-seed content and assert it survives.
        let f = temp_path("preexisting.mkv");
        std::fs::write(&f, b"keepme").unwrap();
        let r = validate_file_dest(&f);
        let survived = std::fs::read(&f).unwrap_or_default();
        let _ = std::fs::remove_file(&f);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(survived, b"keepme", "existing output must not be truncated");
    }

    #[test]
    fn full_preflight_dest_parent_missing_errors() {
        let missing = temp_path("nodir");
        let dest = format!("mkv://{}", missing.join("m.mkv").display());
        let e = preflight("null://", &dest, false, false).unwrap_err();
        assert!(!e.is_empty());
    }

    // ── predicates ───────────────────────────────────────────────────────────

    #[test]
    fn dest_is_iso_predicate() {
        assert!(dest_is_iso(&parse_url("iso://x.iso")));
        assert!(!dest_is_iso(&parse_url("mkv://x.mkv")));
        assert!(!dest_is_iso(&parse_url("null://")));
    }

    #[test]
    fn scheme_only_sink_predicate() {
        assert!(is_scheme_only_sink(&parse_url("null://")));
        assert!(is_scheme_only_sink(&parse_url("stdio://")));
        assert!(!is_scheme_only_sink(&parse_url("mkv://x.mkv")));
        assert!(!is_scheme_only_sink(&parse_url("iso://x.iso")));
    }

    // ── null:// multi-title routing fix ──────────────────────────────────────

    /// Regression: `null://` on a MULTI-title scanned source must route every
    /// selected title to the bare `null://` sink — NEVER synthesize an invalid
    /// `null://stem_t1.null` (which `parse_url` rejects → output() error, the
    /// old bug). Each emitted job's dest URL must be exactly `null://`, and every
    /// such URL must re-parse to `StreamUrl::Null` (proving it's a valid sink).
    #[test]
    fn null_dest_multi_title_routes_all_to_sink() {
        let titles = Some(vec![
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
        ]);
        let out = Output::new(false, true);
        let parsed = parse_url("null://");
        let jobs = build_jobs(&titles, false, &[], false, "null://", &parsed, &out)
            .expect("null:// multi-title must build jobs, not fail");
        assert_eq!(jobs.len(), 3, "one job per title");
        for (idx, url) in &jobs {
            assert!(idx.is_some(), "each job names its title index");
            assert_eq!(url, "null://", "every title routes to the bare sink");
            assert!(
                matches!(parse_url(url), libfreemkv::StreamUrl::Null),
                "the sink URL must re-parse to Null (not Unknown): {url}"
            );
        }
    }

    /// `stdio://` (the other scheme-only sink) gets the same multi-title routing.
    #[test]
    fn stdio_dest_multi_title_routes_all_to_sink() {
        let titles = Some(vec![
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
        ]);
        let out = Output::new(false, true);
        let parsed = parse_url("stdio://");
        let jobs = build_jobs(&titles, false, &[], false, "stdio://", &parsed, &out)
            .expect("stdio:// multi-title must build jobs");
        assert_eq!(jobs.len(), 2);
        for (_idx, url) in &jobs {
            assert_eq!(url, "stdio://");
        }
    }

    /// `demux://` multi-title routing: each title gets its own
    /// `demux://<dir>/t<NN>/` subdir, and (regression) every job URL must carry
    /// the `demux://` scheme so it re-parses to `Demux` — NOT bare `out/tNN/`,
    /// which `parse_url` rejects as Unknown and `output()` then errors on.
    #[test]
    fn demux_dest_multi_title_urls_carry_scheme() {
        let titles = Some(vec![
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
        ]);
        let out = Output::new(false, true);
        let parsed = parse_url("demux://out/");
        let jobs = build_jobs(&titles, false, &[], false, "demux://out/", &parsed, &out)
            .expect("demux:// multi-title must build jobs");
        assert_eq!(jobs.len(), 2, "one job per title");
        // t01 / t02 subdirs, each a valid Demux URL (scheme present).
        assert_eq!(jobs[0].1, "demux://out/t01/");
        assert_eq!(jobs[1].1, "demux://out/t02/");
        for (idx, url) in &jobs {
            assert!(idx.is_some(), "each job names its title index");
            assert!(
                matches!(parse_url(url), libfreemkv::StreamUrl::Demux { .. }),
                "the job URL must re-parse to Demux (not Unknown): {url}"
            );
        }
    }

    /// A real file dest (mkv://) on a multi-title source still routes through
    /// per-title naming (the sink special-case must NOT swallow file dests).
    #[test]
    fn file_dest_multi_title_still_named_per_title() {
        let mut t0 = libfreemkv::DiscTitle::empty();
        t0.playlist = "Movie".into();
        let titles = Some(vec![t0, libfreemkv::DiscTitle::empty()]);
        let out = Output::new(false, true);
        // Directory dest (trailing slash) → one named file per title.
        let dir = temp_path("mkvout");
        let dest = format!("{}/", dir.display());
        let parsed = parse_url(&format!("mkv://{}/", dir.display()));
        let jobs = build_jobs(&titles, false, &[], true, &dest, &parsed, &out);
        let _ = std::fs::remove_dir_all(&dir);
        let jobs = jobs.expect("dir dest builds per-title jobs");
        assert_eq!(jobs.len(), 2);
        for (_idx, url) in &jobs {
            assert!(url.contains("_t"), "per-title file naming preserved: {url}");
        }
    }

    /// `preflight_validate` must NEVER panic, on any combination of adversarial
    /// scheme strings × flag states. The only acceptable outcomes are Ok or Err
    /// — a panic here would crash the CLI on malformed input.
    #[test]
    fn preflight_never_panics_on_adversarial_combinations() {
        let urls = [
            "",
            "://",
            "disc://",
            "disc:///dev/null",
            "iso://",
            "iso://\0",
            "mkv://",
            "m2ts://x",
            "null://",
            "null://trailing",
            "stdio://",
            "network://",
            "network://host:9000",
            "gopher://x",
            "out.mkv",
            "/abs/path",
            "iso://日本語.iso",
            &"iso://".to_string().repeat(1000),
        ];
        for &s in &urls {
            for &d in &urls {
                for raw in [false, true] {
                    for mp in [false, true] {
                        // Must return (Ok or Err), never panic.
                        let _ = preflight(s, d, raw, mp);
                    }
                }
            }
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // WS3 — dropped `-k` short flag (rc.6: `--keydb` long form ONLY).
    //
    // The `-k` short flag was removed from `parse_flags`. It must now be
    // treated as an UNKNOWN flag (hard error), NOT silently consume its value
    // as a keydb path — otherwise a user who learned `-k` in an earlier rc would
    // get a confusing "unexpected positional" downstream, or worse, the value
    // would be eaten and the rip would proceed against the default keydb.
    // ════════════════════════════════════════════════════════════════════════

    /// `-k <path>` is no longer a recognized flag: it must be rejected as an
    /// unknown flag (so the user is told to use `--keydb`), never quietly
    /// accepted. The long `--keydb` form (tested elsewhere) is the only spelling.
    #[test]
    fn dropped_short_k_flag_is_unknown() {
        // `-k keydb.cfg` — the dropped short form — must error.
        let err = parse_flags(&v(&["-k", "keydb.cfg"])).unwrap_err();
        assert!(
            err.contains("-k"),
            "unknown-flag error must name `-k`: {err}"
        );
        // It must NOT have been parsed as a keydb path (the parse failed, so no
        // ParsedFlags exists — but assert the long form still works to prove we
        // didn't break `--keydb` while dropping `-k`).
        let f = parse_flags(&v(&["--keydb", "keydb.cfg"])).unwrap();
        assert_eq!(f.keydb_path.as_deref(), Some("keydb.cfg"));
        // Bare `-k` (no value) is likewise unknown, not "needs a value".
        let err = parse_flags(&v(&["-k"])).unwrap_err();
        assert!(
            err.contains("-k") && !err.contains("requires a value"),
            "bare `-k` must be unknown-flag, not flag_needs_value: {err}"
        );
    }

    /// The dropped `--device` / `-d` flags: the device now comes from the source
    /// URL (`disc:///dev/sgN`). `parse_flags` must reject `--device`/`-d` as
    /// unknown — they are not silently swallowed (which would let a stray device
    /// path leak through as a positional).
    #[test]
    fn dropped_device_flags_are_unknown() {
        for bad in [
            v(&["--device", "/dev/sg0"]),
            v(&["-d", "/dev/sg0"]),
            v(&["--device"]),
            v(&["-d"]),
        ] {
            match parse_flags(&bad) {
                Ok(f) => panic!("{bad:?} must be rejected, got {f:?}"),
                Err(err) => assert!(!err.is_empty(), "{bad:?}: unknown-flag error must be set"),
            }
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // WS3 — device comes from the source URL (`disc:///dev/sgN`).
    //
    // `main::info_cmd` / `pipe_disc` / `dir_to_extract` all read the device out
    // of the parsed `disc://` URL — there is no `--device` flag anymore. Pin the
    // exact `parse_url` shape those routes depend on, so a parser change that
    // breaks device-from-URL is caught here in the CLI's own tests.
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn device_comes_from_disc_url() {
        // Explicit device path → carried in the URL.
        match parse_url("disc:///dev/sg3") {
            libfreemkv::StreamUrl::Disc { device: Some(p) } => {
                assert_eq!(p.to_string_lossy(), "/dev/sg3");
            }
            other => panic!("disc:///dev/sg3 must parse to Disc{{device:Some}}, got {other:?}"),
        }
        // Bare `disc://` → auto-detect (device None); the routes fall back to
        // `find_drive()` rather than a flag.
        match parse_url("disc://") {
            libfreemkv::StreamUrl::Disc { device: None } => {}
            other => panic!("disc:// must parse to Disc{{device:None}}, got {other:?}"),
        }
        // A Windows device path survives the URL too (the route is OS-agnostic;
        // the path string is opaque to the parser).
        match parse_url("disc://D:") {
            libfreemkv::StreamUrl::Disc { device: Some(p) } => {
                assert_eq!(p.to_string_lossy(), "D:");
            }
            other => panic!("disc://D: must parse to Disc{{device:Some}}, got {other:?}"),
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // WS3 — path handling (Windows-relevant). `sanitize_name` seeds per-title
    // output filenames under a directory dest; it must never emit a path
    // separator, a host-illegal character, or an empty stem — on ANY platform,
    // since a rip authored on Linux may be muxed on Windows. These run on every
    // platform (the function is platform-agnostic by design).
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn sanitize_name_strips_path_separators_and_illegal_chars() {
        // Forward AND back slashes must not survive — a `Movie/Part2` stem would
        // otherwise synthesize `dir/Movie/Part2_t1.mkv`, escaping the dest dir.
        // The separator is STRIPPED (not converted), so the space-only gaps are
        // what become underscores.
        let s = sanitize_name("Movie/Part 2");
        assert!(!s.contains('/'), "path separator survived: {s}");
        assert_eq!(s, "MoviePart_2", "got {s}");
        let s = sanitize_name(r"A\B");
        assert!(!s.contains('\\'), "backslash survived: {s}");
        assert_eq!(s, "AB", "backslash stripped, not converted: {s}");
        // Windows-illegal punctuation (`: * ? " < > |`) is dropped, not kept.
        let s = sanitize_name(r#"a:b*c?d"e<f>g|h"#);
        for bad in [':', '*', '?', '"', '<', '>', '|', '\\', '/'] {
            assert!(!s.contains(bad), "illegal char {bad:?} survived in {s}");
        }
        assert_eq!(s, "abcdefgh", "got {s}");
    }

    #[test]
    fn sanitize_name_spaces_to_underscores_and_trims() {
        assert_eq!(sanitize_name("  The  Movie  "), "The__Movie");
        // Hyphen and underscore are preserved (legal everywhere).
        assert_eq!(sanitize_name("Director-Cut_2"), "Director-Cut_2");
    }

    #[test]
    fn sanitize_name_empty_or_all_illegal_falls_back_to_disc() {
        // An empty or fully-stripped stem must fall back to "disc" so the
        // per-title filename is never `_t1.mkv` (leading underscore, no stem).
        assert_eq!(sanitize_name(""), "disc");
        assert_eq!(sanitize_name("///"), "disc");
        assert_eq!(sanitize_name(":*?"), "disc");
        assert_eq!(sanitize_name("   "), "disc");
        // A name that is ALL non-ascii is stripped to empty → "disc".
        assert_eq!(sanitize_name("日本語"), "disc");
    }

    // ════════════════════════════════════════════════════════════════════════
    // WS3 — keydb path resolution (Windows-relevant). `resolved_keydb_path`
    // honors an explicit `--keydb` override verbatim, and falls back to the
    // exe-local / default location otherwise. It must NEVER panic and ALWAYS
    // return a usable path (the bare `keydb.cfg` last resort guarantees Some).
    // The exe-relative search policy itself is owned + tested by
    // `freemkv-keysources::paths`; this pins the CLI's wrapper behavior.
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn resolved_keydb_path_honors_explicit_override() {
        // An explicit `--keydb PATH` is used verbatim, never the search policy.
        let p = resolved_keydb_path(&Some("/custom/keydb.cfg".to_string()));
        assert_eq!(p, std::path::PathBuf::from("/custom/keydb.cfg"));
        // A Windows-style override path is passed through unchanged too.
        let p = resolved_keydb_path(&Some(r"C:\keys\keydb.cfg".to_string()));
        assert_eq!(p, std::path::PathBuf::from(r"C:\keys\keydb.cfg"));
    }

    #[test]
    fn resolved_keydb_path_falls_back_without_panicking() {
        // No override → the exe-local/default policy (or the bare `keydb.cfg`
        // last resort). Either way a non-empty path is returned, never a panic.
        let p = resolved_keydb_path(&None);
        assert!(
            p.file_name().is_some_and(|n| n == "keydb.cfg"),
            "fallback must end in keydb.cfg: {}",
            p.display()
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // WS3 — dir:// preflight messaging render. The dir:// validation strings
    // carry placeholders; pin that the RENDERED message substitutes them (no
    // leftover `{path}` / `{source}` braces) and surfaces actionable guidance.
    // The gating (which inputs error) is covered by the dir:// gate tests above;
    // this covers the user-facing TEXT.
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn dir_source_unsupported_message_substitutes_and_guides() {
        // A byte-stream source into dir:// renders the localized guidance with
        // the offending source URL substituted and no leftover placeholder.
        let out = temp_path("dir_msg_src");
        let dest = format!("dir://{}/", out.display());
        let err = preflight("mkv://in.mkv", &dest, false, false).unwrap_err();
        let _ = std::fs::remove_dir_all(&out);
        assert!(
            err.contains("mkv://in.mkv"),
            "source not substituted: {err}"
        );
        assert!(!err.contains("{source}"), "leftover placeholder: {err}");
        // Guides toward a usable source (disc:// or iso://).
        assert!(
            err.contains("disc://") || err.contains("iso://"),
            "must guide to a filesystem source: {err}"
        );
    }

    #[test]
    fn dir_dest_is_file_message_substitutes_path() {
        // A dir:// target that is a regular file renders the path-substituted
        // file/folder mismatch message with no leftover `{path}`.
        let f = temp_path("dir_msg_file");
        std::fs::write(&f, b"x").unwrap();
        let dest = format!("dir://{}", f.display());
        let err = preflight("disc://", &dest, false, false).unwrap_err();
        let _ = std::fs::remove_file(&f);
        assert!(
            err.contains(&f.display().to_string()),
            "path not substituted: {err}"
        );
        assert!(!err.contains("{path}"), "leftover placeholder: {err}");
    }

    // ════════════════════════════════════════════════════════════════════════
    // WS3 — the new (WS2) messaging render shape. `main::fatal` builds the fatal
    // block from `error.fatal_header` (`{level}: {op} failed: {cause}`) and the
    // `error.fatal_diagnostic_hint`. `fmt_err` produces the code-forward
    // `{cause}` fragment. Pin the assembled shape end-to-end so a locale or
    // template change that drops the code, the level word, or a placeholder is
    // caught.
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn fatal_header_assembles_level_op_and_code_forward_cause() {
        // The cause fragment for a real library error, code-forward (E-prefixed).
        let cause = fmt_err(&"E6009");
        assert!(
            cause.starts_with("E6009 "),
            "cause not code-forward: {cause}"
        );

        // The render site assembles `{level}: {op} failed: {cause}`. Reproduce
        // the exact substitution `main::fatal` performs (it isn't callable — it
        // exits the process — so we pin the template + parts it feeds).
        let level = crate::strings::get(crate::messaging::Level::Error.locale_key());
        let op = crate::strings::get("error.op_rip");
        let header = crate::strings::fmt(
            "error.fatal_header",
            &[("level", &level), ("op", &op), ("cause", &cause)],
        );
        // All three parts present, in order, with no leftover placeholders.
        assert!(
            header.starts_with(&format!("{level}:")),
            "level first: {header}"
        );
        assert!(header.contains(&op), "op missing: {header}");
        assert!(header.contains(&cause), "cause missing: {header}");
        assert!(
            !header.contains("{level}") && !header.contains("{op}") && !header.contains("{cause}"),
            "leftover placeholder in fatal header: {header}"
        );
        // The diagnostic-log hint exists and names the --log-level escape hatch.
        let hint = crate::strings::get("error.fatal_diagnostic_hint");
        assert_ne!(hint, "error.fatal_diagnostic_hint", "hint key missing");
        assert!(
            hint.contains("--log-level"),
            "hint must point at the log flag: {hint}"
        );
    }

    /// Each operation name key the fatal block can use (`op_rip`, `op_info`,
    /// `op_verify`, `op_update_keys`) must resolve to a real localized word, not
    /// the bare dotted key — otherwise the fatal header reads
    /// `Error: error.op_rip failed: ...`.
    #[test]
    fn fatal_operation_keys_all_resolve() {
        for key in [
            "error.op_rip",
            "error.op_info",
            "error.op_verify",
            "error.op_update_keys",
        ] {
            assert_ne!(
                crate::strings::get(key),
                key,
                "fatal op key {key} unresolved (would print the raw key)"
            );
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // WS3 — Windows-only compile-gated coverage. `cfg(windows)` does NOT run in
    // the Mac precommit, but it MUST compile-gate cleanly so CI (which builds on
    // Windows) validates it. These pin the OS-specific path/keydb shapes the
    // CLI relies on under Windows.
    // ════════════════════════════════════════════════════════════════════════

    #[cfg(windows)]
    #[test]
    fn windows_keydb_override_keeps_drive_letter_path() {
        // A Windows override path (drive letter + backslashes) must survive
        // verbatim through the CLI wrapper, unmangled.
        let p = resolved_keydb_path(&Some(r"C:\Users\me\AppData\keydb.cfg".to_string()));
        assert_eq!(
            p,
            std::path::PathBuf::from(r"C:\Users\me\AppData\keydb.cfg")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_sanitize_name_drops_reserved_punctuation() {
        // On Windows the `:` (drive separator) and `\` MUST never reach a
        // synthesized filename. (The function is platform-agnostic, but pin it
        // explicitly under the Windows build so a regression is caught on CI's
        // Windows job even if a future change made it cfg-specific.)
        let s = sanitize_name(r"C:\Movie");
        assert!(!s.contains(':') && !s.contains('\\'), "got {s}");
    }
}
