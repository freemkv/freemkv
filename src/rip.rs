//! freemkv rip — Back up a disc.

use crate::strings;
use crate::output::{Output, Level::{Normal, Verbose}};
use std::io::{BufWriter, Write};
use libfreemkv::IOStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// I/O buffer size for file read/write (4 MB).
const IO_BUF_SIZE: usize = 4 * 1024 * 1024;

/// MKV lookahead buffer for codec header detection (10 MB).
const MKV_LOOKAHEAD: usize = 10 * 1024 * 1024;

fn install_signal_handler() {
    #[cfg(unix)]
    unsafe { libc::signal(libc::SIGINT, handle_sigint as *const () as libc::sighandler_t); }

    #[cfg(windows)]
    unsafe {
        extern "system" fn handler(_: u32) -> i32 {
            INTERRUPTED.store(true, Ordering::Relaxed);
            1 // TRUE = handled
        }
        extern "system" { fn SetConsoleCtrlHandler(handler: unsafe extern "system" fn(u32) -> i32, add: i32) -> i32; }
        SetConsoleCtrlHandler(handler, 1);
    }
}

#[cfg(unix)]
extern "C" fn handle_sigint(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

pub fn run(args: &[String]) {
    let mut device_path: Option<String> = None;
    let mut output_dir: Option<String> = None;
    let mut keydb_path: Option<String> = None;
    let mut title_num: Option<usize> = None;
    let mut list_only = false;
    let mut raw_mode = false;
    let mut verbose = false;
    let mut quiet = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--device" | "-d" => { i += 1; device_path = args.get(i).cloned(); }
            "--output" | "-o" => { i += 1; output_dir = args.get(i).cloned(); }
            "--keydb" | "-k" => { i += 1; keydb_path = args.get(i).cloned(); }
            "--title" | "-t" => { i += 1; title_num = args.get(i).and_then(|s| s.parse().ok()); }
            "--list" | "-l" => { list_only = true; }
            "--raw" => { raw_mode = true; }
            "--verbose" | "-v" => { verbose = true; }
            "--quiet" | "-q" => { quiet = true; }
            _ => {
                if device_path.is_none() && args[i].starts_with("/dev/") {
                    device_path = Some(args[i].clone());
                }
            }
        }
        i += 1;
    }

    let device = match device_path {
        Some(p) => match libfreemkv::resolve_device(&p) {
            Ok((resolved, Some(w))) => { eprintln!("  {}", w); resolved }
            Ok((resolved, None)) => resolved,
            Err(e) => { eprintln!("{}", e); std::process::exit(1); }
        },
        None => match libfreemkv::find_drive() {
            Some(d) => d,
            None => { eprintln!("{}", strings::get("error.no_drive")); std::process::exit(1); }
        },
    };

    let out = Output::new(verbose, quiet);

    out.raw(Normal, &format!("freemkv rip v{}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);

    // Open drive
    out.raw_inline(Normal, &format!("{} ", strings::fmt("rip.opening", &[("device", &device)])));
    let mut session = match libfreemkv::DriveSession::open(Path::new(&device)) {
        Ok(s) => { out.print(Normal, "rip.ok"); s }
        Err(e) => { out.print(Normal, "rip.failed"); eprintln!("  {}", e); std::process::exit(1); }
    };
    out.raw(Normal, &format!("  {} {}", session.drive_id.vendor_id.trim(), session.drive_id.product_id.trim()));

    // Wait for disc
    out.print_inline(Normal, "rip.waiting");
    out.raw_inline(Normal, " ");
    match session.wait_ready() {
        Ok(_) => out.print(Normal, "rip.ok"),
        Err(e) => { out.print(Normal, "rip.failed"); eprintln!("  {}", e); std::process::exit(1); }
    }

    // Init + probe
    out.print_inline(Normal, "rip.initializing");
    out.raw_inline(Normal, " ");
    match session.init() {
        Ok(_) => {
            out.print(Normal, "rip.ok");
            out.print_inline(Normal, "rip.probing");
            out.raw_inline(Normal, " ");
            match session.probe_disc() {
                Ok(_) => out.print(Normal, "rip.ok"),
                Err(e) => { out.print(Normal, "rip.failed"); eprintln!("  {}", e); }
            }
        }
        Err(e) => {
            out.print(Normal, "rip.failed");
            out.fmt(Normal, "rip.continuing_oem", &[("error", &e.to_string())]);
        }
    }

    // Scan
    out.print_inline(Normal, "rip.scanning");
    out.raw_inline(Normal, " ");
    let opts = match keydb_path {
        Some(ref kp) => libfreemkv::ScanOptions::with_keydb(kp),
        None => libfreemkv::ScanOptions::default(),
    };
    let disc = match libfreemkv::Disc::scan(&mut session, &opts) {
        Ok(d) => { out.print(Normal, "rip.ok"); d }
        Err(e) => { out.print(Normal, "rip.failed"); eprintln!("  {}", e); std::process::exit(1); }
    };

    // Disc info
    out.blank(Normal);
    out.raw(Normal, &format!("  {}: {:.1} GB", strings::get("rip.capacity"), disc.capacity_gb()));
    if disc.encrypted {
        if let Some(ref aacs) = disc.aacs {
            out.print(Normal, "rip.aacs_keys_found");
            out.raw(Normal, &format!("  VUK:      {:02x}{:02x}{:02x}{:02x}...",
                aacs.vuk[0], aacs.vuk[1], aacs.vuk[2], aacs.vuk[3]));
            out.blank(Verbose);
            out.raw(Verbose, &format!("  {}:   {}", strings::get("rip.verbose_aacs_version"), aacs.version));
            out.raw(Verbose, &format!("  {}:     {:?}", strings::get("rip.verbose_key_source"), aacs.key_source));
            out.raw(Verbose, &format!("  {}:      {}", strings::get("rip.verbose_disc_hash"), aacs.disc_hash));
            if let Some(v) = aacs.mkb_version {
                out.raw(Verbose, &format!("  {}:    {}", strings::get("rip.verbose_mkb_version"), v));
            }
            out.raw(Verbose, &format!("  {}: {}", strings::get("rip.verbose_bus_encryption"), aacs.bus_encryption));
            out.raw(Verbose, &format!("  {}:      {}", strings::get("rip.verbose_volume_id"), hex(&aacs.volume_id)));
            out.raw(Verbose, &format!("  {}:      {}", strings::get("rip.verbose_unit_keys"), aacs.unit_keys.len()));
            if let Some(ref rdk) = aacs.read_data_key {
                out.raw(Verbose, &format!("  {}:  {:02x}{:02x}{:02x}{:02x}...",
                    strings::get("rip.verbose_read_data_key"), rdk[0], rdk[1], rdk[2], rdk[3]));
            }
            if let Some(ref err) = aacs.handshake_error {
                out.raw(Verbose, &format!("  {}: {} ({})",
                    strings::get("rip.verbose_handshake_error"), err.code(), err));
            }
        } else {
            out.print(Normal, "rip.aacs_no_keys");
            out.raw(Verbose, &format!("  ({})", strings::get("rip.verbose_handshake_hint")));
        }
    }

    // Titles
    out.blank(Normal);
    out.raw(Normal, &format!("{} ({}):", strings::get("rip.titles"), disc.titles.len()));
    out.blank(Normal);
    let target_idx = title_num.unwrap_or(1).saturating_sub(1);
    for (i, title) in disc.titles.iter().enumerate() {
        let marker = if i == target_idx { ">" } else { " " };
        out.raw(Normal, &format!("{} {:2}. {} — {:.1} GB — {}", marker, i + 1,
            title.duration_display(), title.size_gb(), title.playlist));
        if out.enabled(Normal) {
            for stream in &title.streams {
                match stream {
                    libfreemkv::Stream::Video(v) => out.raw(Normal, &format!("       {:?} {}", v.codec, v.resolution)),
                    libfreemkv::Stream::Audio(a) => out.raw(Normal, &format!("       {:?} {} {}", a.codec, a.channels, a.language)),
                    libfreemkv::Stream::Subtitle(s) => out.raw(Normal, &format!("       {}", s.language)),
                }
            }
        }
    }

    if list_only { return; }
    if disc.encrypted && disc.aacs.is_none() {
        eprintln!("\n{}", strings::get("rip.cannot_rip_no_keys"));
        std::process::exit(1);
    }

    let meta = disc.titles[target_idx].clone();

    // Output path
    let out_dir = output_dir.map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let _ = std::fs::create_dir_all(&out_dir);

    let ext = if raw_mode { "m2ts" } else { "mkv" };
    let name = disc.meta_title.as_deref().unwrap_or(&disc.volume_id)
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != ' ' && c != '-' && c != '_', "")
        .trim()
        .replace(' ', "_");
    let filename = if name.is_empty() {
        format!("disc.{}", ext)
    } else {
        format!("{}.{}", name, ext)
    };
    let out_file = out_dir.join(&filename);

    out.blank(Normal);
    out.fmt(Normal, "rip.ripping", &[
        ("num", &(target_idx + 1).to_string()),
        ("duration", &meta.duration_display()),
        ("size", &format!("{:.1}", meta.size_gb())),
        ("file", &out_file.display().to_string()),
    ]);

    if meta.extents.is_empty() {
        eprintln!("{}", strings::get("rip.no_extents")); std::process::exit(1);
    }

    install_signal_handler();

    let file = match std::fs::File::create(&out_file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}", strings::fmt("rip.cannot_create", &[
                ("path", &out_file.display().to_string()),
                ("error", &e.to_string()),
            ]));
            std::process::exit(1);
        }
    };

    let total_bytes = meta.size_bytes;
    let start = std::time::Instant::now();

    // Build the output stream chain
    if raw_mode {
        let raw = libfreemkv::M2tsStream::new(BufWriter::with_capacity(IO_BUF_SIZE, file))
            .meta(&meta);
        let mut output = ProgressWriter::new(raw, total_bytes, &INTERRUPTED);
        let mut reader = disc.open_title(&mut session, target_idx).unwrap();
        rip_loop(&mut reader, &mut output);
        let _ = output.inner.finish();
        print_summary(&out_file, start, output.bytes_written, output.peak_speed);
    } else {
        let mkv = libfreemkv::MkvStream::new(BufWriter::with_capacity(IO_BUF_SIZE, file))
            .meta(&meta)
            .max_buffer(MKV_LOOKAHEAD);
        let mut output = ProgressWriter::new(mkv, total_bytes, &INTERRUPTED);
        let mut reader = disc.open_title(&mut session, target_idx).unwrap();
        rip_loop(&mut reader, &mut output);
        let _ = output.inner.finish();
        print_summary(&out_file, start, output.bytes_written, output.peak_speed);
    }
}

fn rip_loop(reader: &mut libfreemkv::ContentReader, output: &mut impl Write) {
    loop {
        if INTERRUPTED.load(Ordering::Relaxed) { break; }
        match reader.read_batch() {
            Ok(Some(batch)) => {
                if output.write_all(batch).is_err() { break; }
            }
            Ok(None) => break,
            Err(e) => eprintln!("\n{}", strings::fmt("rip.read_error", &[("error", &e.to_string())])),
        }
    }
}

fn print_summary(path: &std::path::Path, start: std::time::Instant, bytes: u64, peak: f64) {
    let elapsed = start.elapsed().as_secs_f64();
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let (sz, unit) = if mb >= 1024.0 { (mb / 1024.0, "GB") } else { (mb, "MB") };
    let time = format!("{}:{:02}", (elapsed / 60.0) as u32, (elapsed % 60.0) as u32);
    println!("\n");
    println!("{}", strings::fmt("rip.complete", &[
        ("size", &format!("{:.1}", sz)), ("unit", unit), ("time", &time),
    ]));
    println!("{}", strings::fmt("rip.speed", &[
        ("avg", &format!("{:.1}", mb / elapsed)),
        ("peak", &format!("{:.1}", peak)),
    ]));
    println!("{}", strings::fmt("rip.output", &[("path", &path.display().to_string())]));
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join("")
}

/// Progress wrapper — sits between rip and output, tracks bytes and prints status.
struct ProgressWriter<W: Write> {
    inner: W,
    bytes_written: u64,
    total_bytes: u64,
    start: std::time::Instant,
    last_update: std::time::Instant,
    last_bytes: u64,
    peak_speed: f64,
    interrupt_flag: &'static AtomicBool,
}

impl<W: Write> ProgressWriter<W> {
    fn new(inner: W, total_bytes: u64, interrupt_flag: &'static AtomicBool) -> Self {
        let now = std::time::Instant::now();
        Self { inner, bytes_written: 0, total_bytes, start: now, last_update: now, last_bytes: 0, peak_speed: 0.0, interrupt_flag }
    }
}

impl<W: Write> Write for ProgressWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.interrupt_flag.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "interrupted"));
        }
        let n = self.inner.write(buf)?;
        self.bytes_written += n as u64;

        let now = std::time::Instant::now();
        if now.duration_since(self.last_update).as_secs_f64() >= 2.0 {
            let elapsed = self.start.elapsed().as_secs_f64();
            let mb = self.bytes_written as f64 / (1024.0 * 1024.0);
            let avg = mb / elapsed;
            let interval = now.duration_since(self.last_update).as_secs_f64();
            let recent = (self.bytes_written - self.last_bytes) as f64 / (1024.0 * 1024.0) / interval;
            if recent > self.peak_speed { self.peak_speed = recent; }
            let pct = if self.total_bytes > 0 { (self.bytes_written as f64 / self.total_bytes as f64 * 100.0).min(100.0) } else { 0.0 };
            let eta = if avg > 0.0 && self.total_bytes > 0 {
                let s = (self.total_bytes - self.bytes_written) as f64 / (1024.0 * 1024.0) / avg;
                format!("{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32)
            } else { "--:--".into() };
            let total_mb = self.total_bytes as f64 / (1024.0 * 1024.0);
            let (d, t) = if total_mb >= 1024.0 {
                (format!("{:.1} GB", mb / 1024.0), format!("{:.1} GB", total_mb / 1024.0))
            } else {
                (format!("{:.0} MB", mb), format!("{:.0} MB", total_mb))
            };
            eprint!("\r  {} / {}  ({:.0}%)  {:.1} MB/s  ETA {}   ", d, t, pct, avg, eta);
            let _ = std::io::stderr().flush();
            self.last_update = now;
            self.last_bytes = self.bytes_written;
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}
