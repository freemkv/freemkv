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
            // E7022 names the disc by hash; keep its dedicated placeholder.
            if code_part == "E7022" {
                return strings::fmt(&key, &[("hash", data), ("detail", data)]);
            }
            // E6000 (DiscRead) Display is `E6000: <sector> 0x..hex..` — the
            // status/sense hex tail is diagnostic noise that must not reach the
            // user. Pass ONLY the leading sector number as {detail}.
            if code_part == "E6000" {
                let sector = data.split_whitespace().next().unwrap_or(data);
                return strings::fmt(&key, &[("detail", sector)]);
            }
            return strings::fmt(&key, &[("detail", data)]);
        }
    }
    strings::fmt("error.generic", &[("detail", s)])
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
    keydb_path: Option<String>,
    key_url: Option<String>,
    key_auth: Option<String>,
    title_nums: Vec<usize>,
}

/// Where the CLI looks up AACS keys for a disc, assembled from the key flags.
///
/// libfreemkv does no lookup — the CLI resolves a [`libfreemkv::Key`] from these
/// sources and hands it to `Disc::decrypt_with`. The sources are ordered
/// **local-first**: a present keydb is consulted before the network, so an
/// offline hit never makes a key-service round-trip. See [`build_key_sources`].
#[derive(Default, Debug, Clone)]
pub struct KeyConfig {
    /// `-k`/`--keydb PATH` — local `keydb.cfg` (else the standard location).
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
/// - `-k`/`--keydb` with a missing value (never silently use the default).
///
/// A value-flag will not consume a following positional URL token
/// (`scheme://...`) as its value — that means the value is missing.
fn parse_flags(args: &[String]) -> Result<ParsedFlags, String> {
    let mut f = ParsedFlags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // `--log-level N` sets the tracing level (main::init_logging); here
            // it widens prose detail at level >= 2. Swallow its numeric value.
            "--log-level" => {
                if let Some(n) = args.get(i + 1).and_then(|s| s.parse::<u8>().ok()) {
                    f.verbose = n >= 2;
                    i += 1;
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
            "-k" | "--keydb" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(p) if !is_url_token(p) => {
                        i += 1;
                        f.keydb_path = Some(p.clone());
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "-k keydb.cfg")],
                        ));
                    }
                }
            }
            // `--key-url URL` enables the online key service. The URL must not be
            // a positional stream URL token (`scheme://...` other than http(s)) —
            // but a key-service URL IS `https://…`, which `is_url_token` matches
            // on "://". So accept it on its own merit: require an http(s) scheme
            // here, and reject a missing value (next token is a flag, or absent).
            "--key-url" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(u) if is_keyserver_url(u) => {
                        i += 1;
                        f.key_url = Some(u.clone());
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

    // A schemeless dest (e.g. `out.mkv` or `/path/out.mkv`) parses as Unknown.
    // Don't try to use its "scheme" ("unknown") as a file extension / URL scheme
    // (→ `name_t1.unknown`, `unknown://...`) or pass it raw into `output()`
    // (→ cryptic StreamUrlInvalid). Tell the user to add a scheme. Mirrors how
    // `info_cmd` guides on a bad URL.
    if matches!(parsed_dest, libfreemkv::StreamUrl::Unknown { .. }) {
        out.raw(
            Normal,
            &strings::fmt("error.dest_needs_scheme", &[("dest", dest)]),
        );
        return false;
    }

    // `--raw` passes encrypted bytes through unchanged. That is valid for a raw
    // ISO copy (iso:// output) but nonsense for a mux: you cannot mux ciphertext.
    // Reject it up front before building any jobs/pipeline.
    if raw
        && matches!(
            parsed_dest,
            libfreemkv::StreamUrl::Mkv { .. } | libfreemkv::StreamUrl::M2ts { .. }
        )
    {
        out.raw(Normal, &strings::get("error.raw_mux_invalid"));
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

    // Everything else: figure out titles, pipe each one
    // For disc with explicit -t, skip scan_titles (pipe_disc does its own scan)
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });

    // --multipass only governs the disc→ISO sweep (mapfile-driven recovery),
    // which returned above. A direct disc→MKV/M2TS mux is single-pass; honoring
    // multipass would require an ISO intermediate. Warn rather than silently
    // ignore the flag, and point at the supported path.
    if is_disc && multipass {
        out.raw(Normal, &strings::get("rip.multipass_ignored"));
    }
    // For a disc source we skip the upfront `scan_titles` (pipe_disc does its
    // own scan per title); we still need to honor MULTIPLE `-t` flags, so build
    // jobs straight from `title_nums` rather than collapsing to a single title.
    let titles = if is_disc {
        None
    } else {
        scan_titles(source, &keys)
    };
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
    let iso_unit_keys = if is_disc {
        Vec::new()
    } else {
        resolve_iso_unit_keys(source, &keys, &out)
    };

    for (title_idx, dest_url) in &jobs {
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

        if is_disc {
            // Disc source: use open_drive() directly — one session, no double init.
            if let Err(e) = pipe_disc(
                source,
                dest_url,
                title_idx.unwrap_or(0),
                &keys,
                raw,
                multipass,
                &out,
            ) {
                out.raw(Normal, &fmt_err(&e));
                ok = false;
            }
        } else {
            // Non-disc (ISO): hand in the caller-resolved unit keys.
            let opts = libfreemkv::InputOptions {
                unit_keys: iso_unit_keys.clone(),
                title_index: *title_idx,
                raw,
            };
            if let Err(e) = pipe(source, dest_url, &opts, &out) {
                out.raw(Normal, &fmt_err(&e));
                ok = false;
            }
        }
        out.blank(Normal);
    }

    ok
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

    match titles {
        Some(t) if !t.is_empty() => {
            // Scanned source — select which titles.
            let indices: Vec<usize> = if title_nums.is_empty() {
                (0..t.len()).collect()
            } else {
                title_nums.iter().map(|n| n.saturating_sub(1)).collect()
            };
            if indices.len() == 1 && !is_dir_dest {
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
            // one job per requested title, written to a directory. Use a generic
            // "disc" stem (the real disc name isn't known until each per-title
            // scan inside pipe_disc).
            //
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
            let indices: Vec<usize> = title_nums.iter().map(|n| n.saturating_sub(1)).collect();
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
fn drive_scan_opts(keydb_path: &Option<String>) -> libfreemkv::ScanOptions {
    let path = resolved_keydb_path(keydb_path);
    let host_certs = freemkv_keysources::KeydbSource::new(path).host_certs();
    let credentials =
        (!host_certs.is_empty()).then_some(libfreemkv::DriveCredentials { host_certs });
    libfreemkv::ScanOptions {
        credentials,
        ..Default::default()
    }
}

/// Resolve an ISO's AACS unit keys once: keyless scan → local keydb →
/// `decrypt_with`. libfreemkv does no lookup, so the CLI resolves here and the
/// keys ride into each title's stream. Empty for an unencrypted ISO or when no
/// key resolves.
fn resolve_iso_unit_keys(source: &str, keys: &KeyConfig, out: &Output) -> Vec<(u32, [u8; 16])> {
    let path = match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Iso { path } => path,
        _ => return Vec::new(),
    };
    let Ok(mut reader) = libfreemkv::FileSectorSource::open(&path) else {
        return Vec::new();
    };
    let capacity =
        <libfreemkv::FileSectorSource as libfreemkv::SectorSource>::capacity_sectors(&reader);
    let Ok(mut disc) = libfreemkv::Disc::scan_image(&mut reader, capacity, &keyless_scan_opts())
    else {
        return Vec::new();
    };
    // Sample encrypted units from the largest title so key resolution can
    // validate a keydb key against real ciphertext (and reject a wrong one).
    let samples = disc
        .titles
        .iter()
        .max_by_key(|t| t.size_bytes)
        .cloned()
        .map(|t| freemkv_keysources::read_sample_units(&mut reader, &t, SAMPLE_UNITS))
        .unwrap_or_default();
    apply_keys(&mut disc, keys, samples, out);
    match disc.decrypt_keys() {
        libfreemkv::DecryptKeys::Aacs { unit_keys, .. } => unit_keys,
        _ => Vec::new(),
    }
}

/// How many encrypted aligned units to sample for key validation.
const SAMPLE_UNITS: usize = 4;

/// The keydb path to use: `--keydb <path>` if given; else the first
/// per-OS search location that exists (Windows `%APPDATA%\freemkv\keydb.cfg`
/// then the legacy `.config` dotfolder; Linux/macOS `~/.config/freemkv/keydb.cfg`),
/// else the canonical default location for that OS, else a bare `keydb.cfg`
/// in the cwd. The search/default policy lives in `freemkv-keysources`.
fn resolved_keydb_path(keydb_path: &Option<String>) -> std::path::PathBuf {
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
    let mut sources = freemkv_keysources::MultiSource::new(sources);
    freemkv_keysources::resolve_and_apply(&mut sources, &inputs, disc);
}

/// Live-disc analogue of libfreemkv's `mux::resolve::aacs_key_missing`.
/// Returns `true` when decryption is requested (`!raw`), the disc is
/// AACS-encrypted (`has_aacs`), and key resolution yielded no usable key
/// (`DecryptKeys::None`). In that case `pipe_disc` fails fast with
/// `Error::NoDiscKey` rather than streaming ciphertext that muxes to garbage.
/// `--raw` and non-AACS discs (incl. CSS DVDs, which resolve to `Css{..}`)
/// always return `false`.
fn disc_aacs_key_missing(raw: bool, has_aacs: bool, keys: &libfreemkv::DecryptKeys) -> bool {
    !raw && has_aacs && matches!(keys, libfreemkv::DecryptKeys::None)
}

/// Live-disc analogue of libfreemkv's `mux::resolve::css_key_missing`.
/// Returns `true` when decryption is requested (`!raw`), the disc is
/// CSS-encrypted (`has_css`), and per-title key resolution yielded no usable
/// key (`DecryptKeys::None` — e.g. a multi-VTS DVD whose chosen title's VTS
/// could not be re-cracked). In that case `pipe_disc` fails fast with
/// `Error::CssKeyMissing` rather than streaming scrambled MPEG that muxes to
/// garbage. `--raw` and non-CSS discs (AACS resolves to `Aacs{..}`,
/// unencrypted has no `disc.css`) always return `false`.
fn disc_css_key_missing(raw: bool, has_css: bool, keys: &libfreemkv::DecryptKeys) -> bool {
    !raw && has_css && matches!(keys, libfreemkv::DecryptKeys::None)
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
        .map(|t| freemkv_keysources::read_sample_units(&mut drive, &t, SAMPLE_UNITS))
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

    // CSS scrambled-but-uncracked gate (Fix 6): the scan saw scrambled sectors
    // but recovered no title key (`disc.css` is None yet `disc.css_error` is
    // set). The disc IS encrypted; muxing would pass scrambled MPEG through as
    // plaintext (garbage at exit 0). Fail loudly. `--raw` skips decryption.
    if !raw && disc.css_error.is_some() {
        return Err(libfreemkv::Error::CssKeyMissing.to_string());
    }

    let title = disc.titles[title_idx].clone();
    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());

    // Per-title key resolution (Theme B fix, mirrors the ISO path in
    // libfreemkv mux/resolve.rs:321-331). The disc-wide `decrypt_keys()`
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
    let keys = disc.decrypt_keys_for_title(title_idx, &mut drive, batch);

    // CSS no-key gate (parallel to the AACS gate below; mirrors the ISO
    // path's `css_key_missing` at mux/resolve.rs:329). On a CSS DVD whose
    // chosen title's VTS could not be re-cracked, `keys` is
    // `DecryptKeys::None`; muxing that would pass scrambled MPEG through as
    // plaintext (garbage at exit 0). Fail loudly with `Error::CssKeyMissing`
    // instead. `--raw` skips decryption so it is never gated; non-CSS discs
    // (AACS resolves to `Aacs{..}`, unencrypted has no `disc.css`) don't fire.
    if disc_css_key_missing(raw, disc.css.is_some(), &keys) {
        return Err(libfreemkv::Error::CssKeyMissing.to_string());
    }

    // No-key guard (mirrors the ISO-path gate in libfreemkv mux/resolve.rs):
    // if decryption is wanted (not --raw) and the disc is AACS-encrypted but key
    // resolution yielded no usable key, FAIL here. Without this the live disc://
    // path passes `DecryptKeys::None` into `DiscStream`, which then passes
    // ciphertext through unchanged — the demuxer sees no TS syncs, emits nothing,
    // and we write an empty/garbage MKV and exit 0 with no message. Return the
    // `Error::NoDiscKey` Display (`E7022[: <hash>]`) so the caller's `fmt_err`
    // renders "no KEYDB.cfg found / no key for disc" and the exit code is nonzero.
    // CSS DVDs resolve to `DecryptKeys::Css{..}` (never `None`), so this is
    // AACS-only via `disc.aacs`.
    if disc_aacs_key_missing(raw, disc.aacs.is_some(), &keys) {
        let disc_hash = disc
            .aacs
            .as_ref()
            .map(|a| a.disc_hash.trim_start_matches("0x").to_string())
            .unwrap_or_default();
        return Err(libfreemkv::Error::NoDiscKey { disc_hash }.to_string());
    }

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
    // nothing). Surface `Error::NoStreams` (as the ISO path does at libfreemkv
    // mux/resolve.rs:305) so the exit code is nonzero and the user sees a
    // localized message instead of a header-only "success".
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
    // nothing). Surface `Error::NoStreams` (as the ISO path does at libfreemkv
    // mux/resolve.rs:305) so the exit code is nonzero and the user sees a
    // localized message instead of a header-only "success".
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
                out.raw(Normal, &fmt_err(&e));
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
        .map(|t| freemkv_keysources::read_sample_units(&mut drive, &t, SAMPLE_UNITS))
        .unwrap_or_default();
    apply_keys(&mut disc, keys, samples, out);

    let disc_name = sanitize_name(disc.meta_title.as_deref().unwrap_or(&disc.volume_id));
    let (iso_path, is_null) = match &parsed_dest {
        libfreemkv::StreamUrl::Iso { path } => (path.clone(), false),
        libfreemkv::StreamUrl::Null => {
            let p = std::path::PathBuf::from("/dev/null");
            (p, true)
        }
        _ => unreachable!(),
    };

    let total_bytes = disc.capacity_sectors as u64 * 2048;
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
            out.raw(Normal, &fmt_err(&e));
            false
        }
    };

    drive.unlock_tray();
    success
}

// ── Title scanning ──────────────────────────────────────────────────────────

/// Scan any source for its title list. Returns None if source has no titles
/// (e.g. a single M2TS file, network stream).
fn scan_titles(source: &str, keys: &KeyConfig) -> Option<Vec<libfreemkv::DiscTitle>> {
    let parsed = libfreemkv::parse_url(source);

    match parsed {
        libfreemkv::StreamUrl::Iso { ref path } => {
            // Listing needs only clear UDF navigation — no handshake, no creds.
            let mut reader = libfreemkv::FileSectorSource::open(path).ok()?;
            let capacity =
                <libfreemkv::FileSectorSource as libfreemkv::SectorSource>::capacity_sectors(
                    &reader,
                );
            let disc =
                libfreemkv::Disc::scan_image(&mut reader, capacity, &keyless_scan_opts()).ok()?;
            Some(disc.titles)
        }
        libfreemkv::StreamUrl::Disc { ref device } => {
            let mut drive = match device {
                Some(d) => libfreemkv::Drive::open(d).ok()?,
                None => libfreemkv::find_drive()?,
            };
            debug_drive_step("wait_ready", drive.wait_ready());
            debug_drive_step("init", drive.init());
            // probe_disc is advisory: routinely fails (no disc, already probed);
            // the scan below re-derives what it needs, so its result stays dropped.
            let _ = drive.probe_disc();
            // Live drive may be locked → supply handshake credentials.
            let disc =
                libfreemkv::Disc::scan(&mut drive, &drive_scan_opts(keys.keydb_path())).ok()?;
            Some(disc.titles)
        }
        _ => None,
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
    let bytes_worst_case = p
        .bytes_unreadable_total
        .saturating_add(p.bytes_pending_total);
    let disc_damage_secs = if bytes_worst_case > 0 {
        p.disc_duration_secs
            .filter(|&d| d > 0.0)
            .map(|dur| bytes_worst_case as f64 / bytes_disc as f64 * dur)
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

    let damage = if bytes_worst_case > 0 {
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
    };
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
/// flag value. A value-flag (`-t`, `-k`) must not swallow one of these.
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
        KeyConfig, build_jobs, build_key_sources, copy_should_continue, disc_aacs_key_missing,
        disc_copy_recovered_data, disc_css_key_missing, fmt_err_str, headers_resolved,
        is_keyserver_url, is_url_token, mux_produced_output, mux_was_interrupted, parse_error_code,
        parse_flags, title_in_range,
    };
    use crate::output::Output;

    /// The silent-failure gate (Fix 1): an AACS disc whose key resolution
    /// yielded `DecryptKeys::None` must be flagged so `pipe_disc` returns
    /// `Error::NoDiscKey` (E7022 + nonzero exit) instead of streaming
    /// ciphertext into an empty/garbage MKV at exit 0.
    #[test]
    fn aacs_disc_with_no_key_is_flagged() {
        assert!(
            disc_aacs_key_missing(false, true, &libfreemkv::DecryptKeys::None),
            "AACS disc + no key (not --raw) must trip the no-key gate"
        );
    }

    /// `--raw` deliberately skips decryption — no key needed, never gated.
    #[test]
    fn raw_disc_is_never_gated() {
        assert!(
            !disc_aacs_key_missing(true, true, &libfreemkv::DecryptKeys::None),
            "--raw must bypass the no-key gate even with no key"
        );
    }

    /// A non-AACS disc (unencrypted, or a CSS DVD) legitimately has no AACS
    /// key — it must not be gated.
    #[test]
    fn non_aacs_disc_is_never_gated() {
        assert!(
            !disc_aacs_key_missing(false, false, &libfreemkv::DecryptKeys::None),
            "a disc with no AACS state must not be gated"
        );
    }

    // ── CSS per-title no-key gate (Theme B fix #5) ──────────────────────────

    /// A CSS DVD whose chosen title's per-VTS key could not be re-cracked
    /// (`decrypt_keys_for_title` → None) must trip the gate so `pipe_disc`
    /// returns `Error::CssKeyMissing` instead of streaming scrambled MPEG
    /// (garbage at exit 0).
    #[test]
    fn css_disc_with_no_title_key_is_flagged() {
        assert!(
            disc_css_key_missing(false, true, &libfreemkv::DecryptKeys::None),
            "CSS disc + no per-title key (not --raw) must trip the gate"
        );
    }

    /// A CSS disc WITH a resolved title key proceeds.
    #[test]
    fn css_disc_with_key_is_not_flagged() {
        let css = libfreemkv::DecryptKeys::Css {
            title_key: [0u8; 5],
        };
        assert!(
            !disc_css_key_missing(false, true, &css),
            "a resolved CSS title key must not be gated"
        );
    }

    /// `--raw` skips decryption — the CSS gate never fires.
    #[test]
    fn css_raw_disc_is_never_gated() {
        assert!(
            !disc_css_key_missing(true, true, &libfreemkv::DecryptKeys::None),
            "--raw must bypass the CSS no-key gate"
        );
    }

    /// A non-CSS disc (AACS or unencrypted) never trips the CSS gate.
    #[test]
    fn non_css_disc_is_never_gated() {
        assert!(
            !disc_css_key_missing(false, false, &libfreemkv::DecryptKeys::None),
            "a disc with no CSS state must not be CSS-gated"
        );
    }

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
    /// strings — NO raw `E####` may reach the user. This is the core of the
    /// "english errors for all codes" requirement.
    #[test]
    fn fmt_err_renders_codes_to_english() {
        // E6009 NoStreams — the Theme A zero-output error.
        let s = fmt_err_str("E6009");
        assert_eq!(s, "No streams found.");
        assert!(!s.contains("E6009"), "raw code leaked: {s}");

        // E7023 CssKeyMissing — the Theme B CSS gate error. The user-facing
        // copy is dejargoned: "copy-protected", not "CSS title key".
        let s = fmt_err_str("E7023");
        assert!(s.to_lowercase().contains("copy-protected"), "got: {s}");
        assert!(!s.contains("E7023"), "raw code leaked: {s}");

        // E9023 MuxEmpty — the Theme A m2ts zero-frame error. Dejargoned to
        // "empty file" / "video or audio", not the internal "mux" term.
        let s = fmt_err_str("E9023");
        assert!(s.to_lowercase().contains("empty file"), "got: {s}");
        assert!(!s.contains("E9023"), "raw code leaked: {s}");

        // E5000 with data → {detail} substituted, raw code gone.
        let s = fmt_err_str("E5000: 13");
        assert!(s.contains("13"), "detail not substituted: {s}");
        assert!(!s.contains("E5000"), "raw code leaked: {s}");

        // E7013 Decryption failed.
        assert_eq!(fmt_err_str("E7013"), "Decryption failed.");

        // E7022 names the disc by hash.
        let s = fmt_err_str("E7022: deadbeef");
        assert!(s.contains("deadbeef"), "hash not substituted: {s}");
        assert!(!s.contains("E7022"), "raw code leaked: {s}");
    }

    /// E6000 (DiscRead) Display is `E6000: <sector> 0x..status../0x..sense..`.
    /// The status/sense hex tail is diagnostic noise that must NOT reach the
    /// user — only the sector number is substituted into the localized message.
    #[test]
    fn fmt_err_e6000_strips_status_sense_hex_tail() {
        // Full DiscRead Display: sector + status + sense triple.
        let s = fmt_err_str("E6000: 7476928 0x02/0x03/0x11/0x00");
        assert!(s.contains("7476928"), "sector number lost: {s}");
        assert!(!s.contains("0x"), "raw hex tail leaked to user: {s}");
        assert!(!s.contains("E6000"), "raw code leaked: {s}");
        // Sense-only form (no status byte) also strips the tail.
        let s = fmt_err_str("E6000: 100 0x03/0x11/0x00");
        assert!(s.contains("100") && !s.contains("0x"), "got: {s}");
        // Bare sector (no tail at all) renders cleanly.
        let s = fmt_err_str("E6000: 42");
        assert!(s.contains("42") && !s.contains("0x"), "got: {s}");
    }

    /// A code with NO locale entry falls back to the generic wrapper (which
    /// still localizes the surround) rather than printing a bare code — but it
    /// is the last resort, not the common path.
    #[test]
    fn fmt_err_unknown_code_uses_generic_wrapper() {
        // E1234 has no locale entry; the generic wrapper echoes the raw form
        // inside "Error: {detail}".
        let s = fmt_err_str("E1234: whatever");
        assert!(s.starts_with("Error:"), "got: {s}");
        assert!(
            s.contains("E1234"),
            "generic wrapper keeps the raw detail: {s}"
        );
    }

    /// A non-code error string (e.g. a CLI-side message) passes through the
    /// generic wrapper unchanged.
    #[test]
    fn fmt_err_non_code_string_uses_generic() {
        let s = fmt_err_str("No BD drive found");
        assert_eq!(s, "Error: No BD drive found");
    }

    // ── negative path: no-keydb AACS disc → E7022 surfaced in English ───────

    /// End-to-end negative-path coverage: an AACS disc with no usable key (no
    /// keydb / disc hash not in keydb) trips `disc_aacs_key_missing`, which
    /// makes `pipe_disc` return `Error::NoDiscKey`'s Display (`E7022[: hash]`).
    /// That string must render to the ENGLISH E7022 message via `fmt_err` (so
    /// the user never sees a raw `E7022`) AND `run()` propagates the Err to a
    /// nonzero exit. Here we assert the gate + the message; the exit-code wiring
    /// is exercised by `run()` returning `false` on any `pipe_disc` Err.
    #[test]
    fn no_keydb_aacs_disc_surfaces_e7022_in_english() {
        // The gate fires for an AACS disc with no key (not --raw).
        assert!(disc_aacs_key_missing(
            false,
            true,
            &libfreemkv::DecryptKeys::None
        ));
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
        // English, names the disc by hash, no raw code leaked.
        assert!(rendered.contains("deadbeefcafe"), "hash named: {rendered}");
        assert!(!rendered.contains("E7022"), "raw code leaked: {rendered}");
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
            bytes_total_disc: 0,
            disc_duration_secs: None,
            bytes_bad_in_main_title: 0,
            main_title_duration_secs: None,
            main_title_size_bytes: None,
        };
        let pct = p.work_pct();
        assert!(pct.is_finite(), "work_total==0 must not yield NaN%");
        assert_eq!(pct, 100.0);
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
        // `-k` with no value must not silently fall back to the default keydb.
        assert!(parse_flags(&v(&["-k"])).is_err());
        assert!(parse_flags(&v(&["-k", "disc://"])).is_err());
    }

    #[test]
    fn keydb_value_accepted() {
        let f = parse_flags(&v(&["-k", "/etc/keydb.cfg"])).unwrap();
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

    #[test]
    fn key_auth_missing_value_rejected() {
        assert!(parse_flags(&v(&["--key-auth"])).is_err());
        // A following stream URL means the token was omitted.
        assert!(parse_flags(&v(&["--key-auth", "disc://"])).is_err());
    }

    /// Source assembly per the agreed design. `OnlineSource::needs_samples()` is
    /// `true` and `KeydbSource::needs_samples()` is `false`, so the first source's
    /// `needs_samples` distinguishes online-first from keydb-first ordering.
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
}
