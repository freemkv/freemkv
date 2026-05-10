//! Pipe — stream in, stream out.
//!
//! One pipeline for everything:
//!   1. disc→ISO: Disc::copy() (not a stream)
//!   2. Everything else: input → PES → output, one title at a time
//!
//! Batch (multiple titles) is just a for loop calling pipe() per title.
//!
//! 0.18: the CLI is on the FrameSource / FrameSink trait surface. The
//! `libfreemkv::input` / `libfreemkv::output` URL dispatchers still hand
//! back `Box<dyn pes::Stream>` during the deprecation window; the local
//! `PesSource` / `PesSink` adapters in this file bridge those values
//! into the FrameSource / FrameSink shape so the rest of the pipeline
//! never touches the deprecated trait.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::pes::{FrameSink, FrameSource, PesFrame};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

// ── PesSource / PesSink: local adapters over `libfreemkv::input/output` ─────
//
// `libfreemkv::input` and `libfreemkv::output` return `Box<dyn pes::Stream>`
// during the 0.18 deprecation window. These adapters wrap that returned
// box and expose the new FrameSource / FrameSink trait shape so the rest
// of the CLI never touches the deprecated trait directly. The two
// `#[allow(deprecated)]` markers are scoped to these adapter impls — the
// rest of `pipe.rs` and `cmd/info.rs` see only FrameSource / FrameSink.
//
// `unsafe impl Send`: every concrete `pes::Stream` reachable through
// `libfreemkv::input` / `output` (DiscStream, MkvStream, M2tsStream,
// NetworkStream, NullStream, StdioStream) is itself `Send` — they store
// `Box<dyn Read + Send>` / `Box<dyn Write + Send>` and Send-bounded
// codec parsers. The trait object `dyn pes::Stream` lacks `Send` only
// because adding the bound to a deprecated trait would force a wider
// audit than the 0.18 window wants (see the comment above the
// `Stream + Send => FrameSource` blanket impl in `libfreemkv/src/pes.rs`).
// The CLI is single-threaded; the Send claim is conservative.
//
// When libfreemkv flips `input` / `output` to return `Box<dyn FrameSource>`
// / `Box<dyn FrameSink>` directly (a later libfreemkv slice), these
// adapters collapse to `pub type PesSource = Box<dyn FrameSource>` etc.
// and call sites stay put.
#[allow(deprecated)]
struct PesSource {
    inner: Box<dyn libfreemkv::PesStream>,
}

#[allow(deprecated)]
impl PesSource {
    fn new(inner: Box<dyn libfreemkv::PesStream>) -> Self {
        Self { inner }
    }
}

// SAFETY: see module comment above. All concrete `pes::Stream`
// implementations behind `libfreemkv::input` are Send.
#[allow(deprecated)]
unsafe impl Send for PesSource {}

#[allow(deprecated)]
impl FrameSource for PesSource {
    fn read(&mut self) -> std::io::Result<Option<PesFrame>> {
        libfreemkv::PesStream::read(&mut *self.inner)
    }

    fn info(&self) -> &libfreemkv::DiscTitle {
        libfreemkv::PesStream::info(&*self.inner)
    }

    fn codec_private(&self, track: usize) -> Option<Vec<u8>> {
        libfreemkv::PesStream::codec_private(&*self.inner, track)
    }

    fn headers_ready(&self) -> bool {
        libfreemkv::PesStream::headers_ready(&*self.inner)
    }
}

#[allow(deprecated)]
struct PesSink {
    inner: Box<dyn libfreemkv::PesStream>,
    bytes_written: u64,
}

#[allow(deprecated)]
impl PesSink {
    fn new(inner: Box<dyn libfreemkv::PesStream>) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

// SAFETY: see module comment above. All concrete `pes::Stream`
// implementations behind `libfreemkv::output` are Send.
#[allow(deprecated)]
unsafe impl Send for PesSink {}

#[allow(deprecated)]
impl FrameSink for PesSink {
    fn write(&mut self, frame: &PesFrame) -> std::io::Result<()> {
        self.bytes_written += frame.data.len() as u64;
        libfreemkv::PesStream::write(&mut *self.inner, frame)
    }

    fn finish(self: Box<Self>) -> std::io::Result<()> {
        // Stream::finish takes &mut self, FrameSink::finish takes Box<Self>.
        // Move out of the box, finish, drop.
        let mut inner = self.inner;
        libfreemkv::PesStream::finish(&mut *inner)
    }

    fn info(&self) -> &libfreemkv::DiscTitle {
        libfreemkv::PesStream::info(&*self.inner)
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn install_signal_handler() {
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        );
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
fn fmt_err(e: &dyn std::fmt::Display) -> String {
    strings::fmt("error.generic", &[("detail", &e.to_string())])
}

// ── CLI entry point ─────────────────────────────────────────────────────────

/// Returns true on success, false on error.
pub fn run(source: &str, dest: &str, args: &[String]) -> bool {
    install_signal_handler();

    // Parse flags
    let mut verbose = false;
    let mut quiet = false;
    let mut raw = false;
    let mut multipass = false;
    let mut keydb_path: Option<String> = None;
    let mut title_nums: Vec<usize> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" | "--verbose" => verbose = true,
            "-q" | "--quiet" => quiet = true,
            "--raw" => raw = true,
            "--multipass" => multipass = true,
            "-t" | "--title" => {
                i += 1;
                if let Some(n) = args.get(i).and_then(|s| s.parse::<usize>().ok()) {
                    title_nums.push(n);
                }
            }
            "-k" | "--keydb" => {
                i += 1;
                keydb_path = args.get(i).cloned();
            }
            _ => {}
        }
        i += 1;
    }

    let out = Output::new(verbose, quiet);
    out.raw(
        Normal,
        &crate::style::dim(&format!("freemkv {}", env!("CARGO_PKG_VERSION"))),
    );
    out.blank(Normal);

    let parsed_source = libfreemkv::parse_url(source);
    let parsed_dest = libfreemkv::parse_url(dest);

    // Disc → ISO or Disc → null: use Disc::copy() (not a stream)
    if matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. })
        && matches!(
            parsed_dest,
            libfreemkv::StreamUrl::Iso { .. } | libfreemkv::StreamUrl::Null
        )
    {
        disc_to_iso(source, dest, &keydb_path, raw, multipass, &out);
        return true;
    }

    // Everything else: figure out titles, pipe each one
    // For disc with explicit -t, skip scan_titles (pipe_disc does its own scan)
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });
    let titles = if is_disc && !title_nums.is_empty() {
        None // single title mode — pipe_disc handles scan
    } else {
        scan_titles(source, &keydb_path)
    };
    let is_dir_dest = dest.ends_with('/') || std::path::Path::new(parsed_dest.path_str()).is_dir();

    // Build the list of (title_index, dest_url) pairs
    let jobs: Vec<(Option<usize>, String)> = match &titles {
        Some(t) if !t.is_empty() => {
            // Source has titles — select which ones
            let indices: Vec<usize> = if title_nums.is_empty() {
                (0..t.len()).collect()
            } else {
                title_nums.iter().map(|n| n.saturating_sub(1)).collect()
            };

            if indices.len() == 1 && !is_dir_dest {
                // Single title to a single file
                vec![(Some(indices[0]), dest.to_string())]
            } else {
                // Multiple titles → directory
                let ext = parsed_dest.scheme();
                let dest_dir = std::path::Path::new(parsed_dest.path_str());
                let _ = std::fs::create_dir_all(dest_dir);
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

                indices
                    .iter()
                    .map(|&idx| {
                        let filename = format!("{}_t{}.{}", disc_name, idx + 1, ext);
                        let url = format!("{}://{}", ext, dest_dir.join(filename).display());
                        (Some(idx), url)
                    })
                    .collect()
            }
        }
        _ => {
            // No title list — single pass, no title index
            let idx = title_nums.first().map(|n| n - 1);
            vec![(idx, dest.to_string())]
        }
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
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });

    for (title_idx, dest_url) in &jobs {
        // Print title info if we have it
        if let (Some(idx), Some(t)) = (title_idx, &titles) {
            if *idx >= t.len() {
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
                continue;
            }
            let title = &t[*idx];
            out.raw(
                Normal,
                &crate::style::hl(&strings::fmt(
                    "rip.title_info",
                    &[
                        ("num", &(idx + 1).to_string()),
                        ("duration", &title.duration_display()),
                        ("size", &format!("{:.1}", title.size_gb())),
                    ],
                )),
            );
        }

        if is_disc {
            // Disc source: use open_drive() directly — one session, no double init.
            if let Err(e) = pipe_disc(
                source,
                dest_url,
                title_idx.unwrap_or(0),
                &keydb_path,
                raw,
                multipass,
                &out,
            ) {
                out.raw(Normal, &fmt_err(&e));
                ok = false;
            }
        } else {
            // Non-disc: use input() as before
            let opts = libfreemkv::InputOptions {
                keydb_path: keydb_path.clone(),
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

// ── The pipeline engine ─────────────────────────────────────────────────────

/// Disc source: one open, one scan, one stream. No double init.
fn pipe_disc(
    source: &str,
    dest: &str,
    title_idx: usize,
    keydb_path: &Option<String>,
    raw: bool,
    _multipass: bool,
    out: &Output,
) -> Result<(), String> {
    let parsed = libfreemkv::parse_url(source);
    let device = match &parsed {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => p.clone(),
        _ => libfreemkv::find_drive()
            .map(|d| std::path::PathBuf::from(d.device_path()))
            .ok_or_else(|| "No drive found".to_string())?,
    };

    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    let mut drive = libfreemkv::Drive::open(&device).map_err(|e| format!("{}", e))?;
    let _ = drive.wait_ready();
    let _ = drive.init();
    let _ = drive.probe_disc();
    drive.lock_tray();

    let scan_opts = match keydb_path {
        Some(p) => libfreemkv::ScanOptions {
            keydb_path: Some(p.into()),
        },
        None => libfreemkv::ScanOptions::default(),
    };
    let disc = libfreemkv::Disc::scan(&mut drive, &scan_opts).map_err(|e| format!("{}", e))?;

    if title_idx >= disc.titles.len() {
        return Err(format!(
            "Title {} out of range ({})",
            title_idx + 1,
            disc.titles.len()
        ));
    }

    let title = disc.titles[title_idx].clone();
    let keys = disc.decrypt_keys();
    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());
    let format = disc.content_format;

    let mut input = libfreemkv::DiscStream::new(Box::new(drive), title, keys, batch, format);

    if raw {
        input.set_raw();
    }

    out.raw(Normal, &strings::get("rip.ok"));

    // `DiscStream` is Send and impls the deprecated `pes::Stream`; via the
    // round-1 `Stream + Send => FrameSource` blanket it also satisfies
    // `FrameSource`. The rest of the loop talks to it through the
    // `FrameSource` trait method names, so the deprecation pressure stops
    // at the `DiscStream::new` constructor call.
    let input: &mut dyn FrameSource = &mut input;

    // From here, same as pipe(): headers → output → frame loop
    let mut buffered = Vec::new();
    while !FrameSource::headers_ready(input) {
        match FrameSource::read(input) {
            Ok(Some(frame)) => buffered.push(frame),
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    let info = FrameSource::info(input).clone();
    print_stream_info(out, &info);

    let mut title = info.clone();
    let disc_name = disc.meta_title.as_deref().unwrap_or(&disc.volume_id);
    title.playlist = disc_name.to_string();
    title.codec_privates = (0..info.streams.len())
        .map(|i| FrameSource::codec_private(input, i))
        .collect();

    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", dest)]));
    let raw_output = match libfreemkv::output(dest, &title) {
        Ok(s) => {
            out.raw(Normal, &crate::style::ok(&strings::get("rip.ok")));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };
    let mut output = PesSink::new(raw_output);

    out.blank(Normal);

    let total_bytes = info.size_bytes;
    let start = std::time::Instant::now();
    let mut last_update = start;

    for frame in &buffered {
        FrameSink::write(&mut output, frame).map_err(|e| format!("{}", e))?;
    }

    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            out.blank(Normal);
            out.raw(Normal, &strings::get("rip.interrupted"));
            break;
        }

        match FrameSource::read(input) {
            Ok(Some(frame)) => {
                FrameSink::write(&mut output, &frame).map_err(|e| format!("{}", e))?;

                let now = std::time::Instant::now();
                if !out.is_quiet() && now.duration_since(last_update).as_secs_f64() >= 0.5 {
                    print_progress(output.bytes_written(), total_bytes, 0, &start);
                    last_update = now;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    // Capture the byte count before consuming the sink for finish.
    let done = output.bytes_written();
    FrameSink::finish(Box::new(output)).map_err(|e| format!("{}", e))?;

    if !out.is_quiet() {
        eprint!("\r                                                                    \r");
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
    Ok(())
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
    let raw_input = match libfreemkv::input(source, opts) {
        Ok(s) => {
            out.raw(Normal, &crate::style::ok(&strings::get("rip.ok")));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };
    let mut input = PesSource::new(raw_input);

    // Read frames until codec headers are ready (also parses metadata headers for stdio/network)
    let mut buffered = Vec::new();
    while !FrameSource::headers_ready(&input) {
        match FrameSource::read(&mut input) {
            Ok(Some(frame)) => buffered.push(frame),
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    // Get info after header scanning (stdio/network populate info during read)
    let info = FrameSource::info(&input).clone();
    print_stream_info(out, &info);

    // Build output title with codec_privates from input
    let mut title = info.clone();
    title.codec_privates = (0..info.streams.len())
        .map(|i| FrameSource::codec_private(&input, i))
        .collect();

    // Open output, wrapped with byte counter for progress
    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", dest)]));
    let raw_output = match libfreemkv::output(dest, &title) {
        Ok(s) => {
            out.raw(Normal, &crate::style::ok(&strings::get("rip.ok")));
            s
        }
        Err(e) => {
            out.raw(Normal, &strings::get("rip.failed"));
            return Err(format!("{}", e));
        }
    };
    let mut output = PesSink::new(raw_output);

    out.blank(Normal);

    let total_bytes = info.size_bytes;
    let start = std::time::Instant::now();
    let mut last_update = start;

    // Write buffered frames
    for frame in &buffered {
        FrameSink::write(&mut output, frame).map_err(|e| format!("{}", e))?;
    }

    // Stream remaining frames
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            out.blank(Normal);
            out.raw(Normal, &strings::get("rip.interrupted"));
            break;
        }

        match FrameSource::read(&mut input) {
            Ok(Some(frame)) => {
                FrameSink::write(&mut output, &frame).map_err(|e| format!("{}", e))?;

                let now = std::time::Instant::now();
                if !out.is_quiet() && now.duration_since(last_update).as_secs_f64() >= 0.5 {
                    print_progress(output.bytes_written(), total_bytes, 0, &start);
                    last_update = now;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("{}", e)),
        }
    }

    // Capture the byte count before consuming the sink for finish.
    let done = output.bytes_written();
    FrameSink::finish(Box::new(output)).map_err(|e| format!("{}", e))?;

    if !out.is_quiet() {
        eprint!("\r                                                                    \r");
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
    Ok(())
}

// ── Disc → ISO (raw sector copy, not a stream) ────────────────────────────

fn disc_to_iso(
    source: &str,
    dest: &str,
    keydb_path: &Option<String>,
    raw: bool,
    multipass: bool,
    out: &Output,
) {
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
                return;
            }
        },
        None => match libfreemkv::find_drive() {
            Some(d) => d,
            None => {
                out.raw(Normal, &strings::get("error.no_drive"));
                return;
            }
        },
    };
    out.raw(
        Normal,
        &strings::fmt("rip.drive", &[("device", drive.device_path())]),
    );
    let _ = drive.wait_ready();
    let _ = drive.init();
    let _ = drive.probe_disc();

    let scan_opts = match keydb_path {
        Some(p) => libfreemkv::ScanOptions {
            keydb_path: Some(p.into()),
        },
        None => libfreemkv::ScanOptions::default(),
    };
    let disc = match libfreemkv::Disc::scan(&mut drive, &scan_opts) {
        Ok(d) => d,
        Err(e) => {
            out.raw(
                Normal,
                &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
            );
            return;
        }
    };

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
        bytes_per_sec: f64,
        halt: &'a std::sync::Arc<std::sync::atomic::AtomicU64>,
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

                    print_disc_progress(p, inst_speed, self.bytes_per_sec);
                }
            }
            self.halt.load(Ordering::Relaxed) == 0
        }
    }
    let bytes_per_sec = disc
        .titles
        .first()
        .map(|t| {
            if t.duration_secs > 0.0 {
                t.size_bytes as f64 / t.duration_secs
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);
    let progress = CliProgress {
        out,
        last_update: &last_update,
        last_work_done: &last_work_done,
        last_speed_time: &last_speed_time,
        bytes_per_sec,
        halt: &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };

    // 0.18 round 3: Disc::copy is gone — the CLI dispatches sweep / patch
    // directly based on mapfile state. The library only ships flat verbs;
    // multipass policy ("one invocation = one pass") is the CLI's call.
    let mapfile_path = disc.mapfile_for(&iso_path);
    let dispatch_result: libfreemkv::Result<libfreemkv::disc::CopyResult> =
        if multipass && mapfile_path.exists() {
            // Existing mapfile — read state to pick sweep-resume vs patch.
            match libfreemkv::disc::mapfile::Mapfile::load(&mapfile_path) {
                Ok(map) => {
                    let stats = map.stats();
                    let disc_size = disc.capacity_bytes;
                    let covers_disc = map.total_size() == disc_size;
                    let bad_bytes = stats.bytes_pending + stats.bytes_unreadable;
                    if covers_disc && bad_bytes == 0 {
                        // Already done. Synthesize a CopyResult and short-circuit.
                        Ok(libfreemkv::disc::CopyResult {
                            bytes_total: disc_size,
                            bytes_good: stats.bytes_good,
                            bytes_unreadable: 0,
                            bytes_pending: 0,
                            recovered_this_pass: 0,
                            complete: true,
                            halted: false,
                        })
                    } else if !covers_disc {
                        // Resume sweep (mapfile partial — Pass 1 was interrupted).
                        let opts = libfreemkv::SweepOptions {
                            decrypt: !raw,
                            resume: true,
                            batch_sectors: None,
                            skip_on_error: true,
                            progress: Some(&progress),
                            halt: None,
                        };
                        disc.sweep(&mut drive, &iso_path, &opts)
                    } else if stats.bytes_retryable > 0 {
                        // Patch — bad ranges to retry.
                        let opts = libfreemkv::PatchOptions {
                            decrypt: !raw,
                            block_sectors: Some(1),
                            full_recovery: true,
                            reverse: true,
                            wedged_threshold: 50,
                            progress: Some(&progress),
                            halt: None,
                        };
                        disc.patch(&mut drive, &iso_path, &opts).map(|po| {
                            // Translate PatchOutcome → CopyResult so the
                            // existing post-rip reporting code reads the
                            // same fields.
                            libfreemkv::disc::CopyResult {
                                bytes_total: po.bytes_total,
                                bytes_good: po.bytes_good,
                                bytes_unreadable: po.bytes_unreadable,
                                bytes_pending: po.bytes_pending,
                                recovered_this_pass: po.bytes_recovered_this_pass,
                                complete: po.bytes_pending == 0,
                                halted: po.halted,
                            }
                        })
                    } else {
                        // Catch-all: covers_disc but no retryable +
                        // bad_bytes > 0 (only possible if bytes_unreadable
                        // but no NonTrimmed). Resume sweep — same
                        // fallthrough Disc::copy used.
                        let opts = libfreemkv::SweepOptions {
                            decrypt: !raw,
                            resume: true,
                            batch_sectors: None,
                            skip_on_error: true,
                            progress: Some(&progress),
                            halt: None,
                        };
                        disc.sweep(&mut drive, &iso_path, &opts)
                    }
                }
                Err(e) => Err(libfreemkv::Error::IoError { source: e }),
            }
        } else {
            // No mapfile or single-pass mode: fresh sweep.
            let opts = libfreemkv::SweepOptions {
                decrypt: !raw,
                resume: false,
                batch_sectors: None,
                skip_on_error: multipass,
                progress: Some(&progress),
                halt: None,
            };
            disc.sweep(&mut drive, &iso_path, &opts)
        };

    match dispatch_result {
        Ok(r) => {
            if !out.is_quiet() {
                eprint!("\r                                                                    \r");
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
                let main_title_bad = disc
                    .titles
                    .first()
                    .map(|t| disc.bytes_bad_in_title(&mapfile_path, t))
                    .unwrap_or(0);
                let disc_dur = disc.titles.first().map(|t| t.duration_secs).unwrap_or(0.0);
                let disc_size = disc.capacity_bytes;
                let lost_secs = lost_secs(r.bytes_unreadable, disc_size, Some(disc_dur));
                let main_lost_secs = if main_title_bad > 0 && disc_size > 0 && disc_dur > 0.0 {
                    let main_size = disc
                        .titles
                        .first()
                        .map(|t| t.size_bytes)
                        .unwrap_or(disc_size);
                    main_title_bad as f64 / main_size as f64 * disc_dur
                } else {
                    0.0
                };
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
                if lost_secs > 0.0 {
                    let lost_str = fmt_damage_time(lost_secs);
                    if main_lost_secs > 0.0 && main_lost_secs < lost_secs * 0.99 {
                        let main_str = fmt_damage_time(main_lost_secs);
                        out.raw(
                            Normal,
                            &strings::fmt(
                                "rip.damage_lost",
                                &[("time", &lost_str), ("movie_time", &main_str)],
                            ),
                        );
                    } else if main_lost_secs > 0.0 {
                        out.raw(
                            Normal,
                            &strings::fmt("rip.damage_lost_movie", &[("time", &lost_str)]),
                        );
                    } else {
                        out.raw(
                            Normal,
                            &strings::fmt("rip.damage_lost_simple", &[("time", &lost_str)]),
                        );
                    }
                }
            }
        }
        Err(e) => {
            out.raw(Normal, &fmt_err(&e));
        }
    }

    drive.unlock_tray();
}

// ── Title scanning ──────────────────────────────────────────────────────────

/// Scan any source for its title list. Returns None if source has no titles
/// (e.g. a single M2TS file, network stream).
fn scan_titles(source: &str, keydb_path: &Option<String>) -> Option<Vec<libfreemkv::DiscTitle>> {
    let parsed = libfreemkv::parse_url(source);
    let scan_opts = match keydb_path {
        Some(p) => libfreemkv::ScanOptions {
            keydb_path: Some(p.into()),
        },
        None => libfreemkv::ScanOptions::default(),
    };

    match parsed {
        libfreemkv::StreamUrl::Iso { ref path } => {
            let mut reader =
                libfreemkv::mux::iso::IsoSectorReader::open(&path.to_string_lossy()).ok()?;
            let capacity = reader.capacity();
            let disc = libfreemkv::Disc::scan_image(&mut reader, capacity, &scan_opts).ok()?;
            Some(disc.titles)
        }
        libfreemkv::StreamUrl::Disc { ref device } => {
            let mut drive = match device {
                Some(d) => libfreemkv::Drive::open(d).ok()?,
                None => libfreemkv::find_drive()?,
            };
            let _ = drive.wait_ready();
            let _ = drive.init();
            let _ = drive.probe_disc();
            let disc = libfreemkv::Disc::scan(&mut drive, &scan_opts).ok()?;
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

/// Convert "confirmed unrecoverable bytes" into wall-clock seconds, scaled
/// by disc duration. NonTried (`bytes_pending`) bytes are deliberately *not*
/// considered loss — they're recoverable on a `--resume` run. The 0.18.1
/// CLI conflated the two and reported the entire disc as lost from the
/// start of every clean rip; this helper exists so both the live progress
/// line and the post-rip summary share the same definition.
fn lost_secs(bytes_unreadable: u64, bytes_disc: u64, disc_duration_secs: Option<f64>) -> f64 {
    if bytes_unreadable == 0 || bytes_disc == 0 {
        return 0.0;
    }
    disc_duration_secs
        .filter(|&d| d > 0.0)
        .map(|dur| bytes_unreadable as f64 / bytes_disc as f64 * dur)
        .unwrap_or(0.0)
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

fn print_disc_progress(
    p: &libfreemkv::progress::PassProgress,
    inst_speed_mbps: f64,
    _bytes_per_sec: f64,
) {
    let bytes_disc = p.bytes_total_disc;
    if bytes_disc == 0 {
        return;
    }
    // ONE metric across all pass types: cumulative recovered bytes vs disc
    // total. Same scale every pass; bar always advances toward the same
    // final target. Patch passes still emit useful motion because
    // bytes_good_total grows as boundary sectors flip from NonTrimmed to
    // Finished — even if slowly relative to disc total.
    let gb_done = p.bytes_good_total as f64 / 1_073_741_824.0;
    let gb_total = bytes_disc as f64 / 1_073_741_824.0;
    let pct = (p.bytes_good_total as f64 / bytes_disc as f64 * 100.0).min(100.0);
    let eta = if inst_speed_mbps > 0.01 && p.work_total > p.work_done {
        let remaining_mb = (p.work_total - p.work_done) as f64 / 1_048_576.0;
        fmt_eta(remaining_mb / inst_speed_mbps)
    } else {
        "?:??".into()
    };
    let disc_damage_secs = lost_secs(p.bytes_unreadable_total, bytes_disc, p.disc_duration_secs);
    let title_damage_secs = if p.bytes_bad_in_main_title > 0 {
        p.main_title_duration_secs
            .zip(p.main_title_size_bytes)
            .filter(|&(dur, sz)| dur > 0.0 && sz > 0)
            .map(|(dur, sz)| p.bytes_bad_in_main_title as f64 / sz as f64 * dur)
    } else {
        None
    };

    let damage = if p.bytes_unreadable_total > 0 {
        // Always show BOTH numbers: total disc damage + main-title damage.
        // They may be equal (whole-disc damage that's all in the main
        // movie) — show both anyway so the user can see they matched.
        // When main-title isn't computable (no title metadata), fall back
        // to the disc-only string.
        let disc_str = fmt_damage_time(disc_damage_secs);
        let plain = match title_damage_secs {
            Some(ms) if ms > 0.0 => strings::fmt(
                "rip.damage_lost",
                &[("time", &disc_str), ("movie_time", &fmt_damage_time(ms))],
            ),
            _ => strings::fmt("rip.damage_lost_movie", &[("time", &disc_str)]),
        };
        crate::style::warn(&plain)
    } else {
        crate::style::ok(&strings::get("rip.damage_none"))
    };
    let bar = crate::style::bar(24, pct / 100.0);
    eprint!(
        "\r  {} {} {:.1}/{:.1} GB  {}  ETA {}  {}    ",
        bar,
        crate::style::hl(&format!("{:>5.1}%", pct)),
        gb_done,
        gb_total,
        fmt_speed(inst_speed_mbps),
        eta,
        damage,
    );
    let _ = std::io::stderr().flush();
}

fn print_progress(done: u64, total: u64, resumed_from: u64, start: &std::time::Instant) {
    let elapsed = start.elapsed().as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }
    let mb_done = done as f64 / 1_048_576.0;
    let session_mb = (done - resumed_from) as f64 / 1_048_576.0;
    let avg = session_mb / elapsed;

    if total > 0 {
        let pct = (done as f64 / total as f64 * 100.0).min(100.0);
        let mb_total = total as f64 / 1_048_576.0;
        let eta = if avg > 0.0 {
            let s = (total - done) as f64 / 1_048_576.0 / avg;
            format!("{}:{:02}", s as u64 / 60, s as u64 % 60)
        } else {
            "?:??".into()
        };
        let bar = crate::style::bar(24, pct / 100.0);
        let pct_styled = crate::style::hl(&format!("{:>5.1}%", pct));
        if mb_total >= 1024.0 {
            eprint!(
                "\r  {} {} {:.1}/{:.1} GB  {:.1} MB/s  ETA {}    ",
                bar,
                pct_styled,
                mb_done / 1024.0,
                mb_total / 1024.0,
                avg,
                eta
            );
        } else {
            eprint!(
                "\r  {} {} {:.0}/{:.0} MB  {:.1} MB/s  ETA {}    ",
                bar, pct_styled, mb_done, mb_total, avg, eta
            );
        }
    } else {
        eprint!("\r  {:.1} MB  {:.1} MB/s    ", mb_done, avg);
    }
    let _ = std::io::stderr().flush();
}

fn print_stream_info(out: &Output, meta: &libfreemkv::DiscTitle) {
    // Partition by category so the user can scan Video / Audio /
    // Subtitle separately. Per-section count replaces the previous
    // misleading "Titles: N" total (which was actually a stream count).
    let mut videos: Vec<&libfreemkv::VideoStream> = Vec::new();
    let mut audios: Vec<&libfreemkv::AudioStream> = Vec::new();
    let mut subs: Vec<&libfreemkv::SubtitleStream> = Vec::new();
    for s in &meta.streams {
        match s {
            libfreemkv::Stream::Video(v) => videos.push(v),
            libfreemkv::Stream::Audio(a) => audios.push(a),
            libfreemkv::Stream::Subtitle(s) => subs.push(s),
        }
    }

    if !videos.is_empty() {
        out.raw(
            Normal,
            &format!(
                "  {}",
                crate::style::hl(&format!(
                    "{} ({}):",
                    strings::get("disc.video"),
                    videos.len()
                ))
            ),
        );
        for v in &videos {
            let label = if v.label.is_empty() {
                String::new()
            } else {
                format!(" — {}", v.label)
            };
            out.raw(
                Normal,
                &crate::style::highlight_codecs(&format!(
                    "    {} {}{}",
                    v.codec, v.resolution, label
                )),
            );
        }
    }

    if !audios.is_empty() {
        out.raw(
            Normal,
            &format!(
                "  {}",
                crate::style::hl(&format!(
                    "{} ({}):",
                    strings::get("disc.audio"),
                    audios.len()
                ))
            ),
        );
        for a in &audios {
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
                &crate::style::highlight_codecs(&format!(
                    "    {} {} {}{}",
                    a.codec, a.channels, a.language, label
                )),
            );
        }
    }

    if !subs.is_empty() {
        out.raw(
            Normal,
            &format!(
                "  {}",
                crate::style::hl(&format!(
                    "{} ({}):",
                    strings::get("disc.subtitle"),
                    subs.len()
                ))
            ),
        );
        for s in &subs {
            out.raw(Normal, &format!("    {} {}", s.codec, s.language));
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
    use super::*;

    /// Regression for Anomaly A (0.18.1): NonTried (`bytes_pending`) bytes
    /// must NOT count as data loss. A clean rip in progress with the entire
    /// disc still pending should report 0 lost seconds.
    #[test]
    fn lost_secs_excludes_pending_bytes() {
        // 60 GB disc, 2 hours main feature, none unreadable yet — clean rip
        // mid-flight. Should report ZERO loss (NonTried sectors are still
        // recoverable). 0.18.1 reported `(pending+unreadable)/disc * dur`
        // which gave the full 2-hour duration as "lost".
        let disc = 60u64 * 1_000_000_000;
        assert_eq!(lost_secs(0, disc, Some(7200.0)), 0.0);
    }

    /// Confirmed unrecoverable bytes scale linearly with disc fraction.
    #[test]
    fn lost_secs_scales_with_unreadable_fraction() {
        let disc = 100u64 * 1_000_000_000; // 100 GB
        let dur = 6000.0; // 100 minutes
        // 1 GB unreadable on a 100 GB disc with 100-minute main = 1 minute lost
        let secs = lost_secs(1_000_000_000, disc, Some(dur));
        assert!((secs - 60.0).abs() < 0.01, "expected ~60 secs, got {secs}");
    }

    /// Edge cases: zero inputs and missing duration shouldn't NaN or panic.
    #[test]
    fn lost_secs_handles_edge_cases() {
        assert_eq!(lost_secs(0, 100, Some(60.0)), 0.0);
        assert_eq!(lost_secs(50, 0, Some(60.0)), 0.0);
        assert_eq!(lost_secs(50, 100, None), 0.0);
        assert_eq!(lost_secs(50, 100, Some(0.0)), 0.0);
    }
}
