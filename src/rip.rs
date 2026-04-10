//! freemkv rip — Back up a disc.

use crate::strings;
use std::io::{BufWriter, Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn install_signal_handler() {
    unsafe { libc::signal(libc::SIGINT, handle_sigint as *const () as libc::sighandler_t); }
}
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

    println!("freemkv rip v{}", env!("CARGO_PKG_VERSION"));
    println!();

    // Open drive
    let ok = strings::get("rip.ok");
    let failed = strings::get("rip.failed");

    print!("{} ", strings::fmt("rip.opening", &[("device", &device)]));
    let _ = stdout().flush();
    let mut session = match libfreemkv::DriveSession::open(Path::new(&device)) {
        Ok(s) => { println!("{}", ok); s }
        Err(e) => { println!("{}", failed); eprintln!("  {}", e); std::process::exit(1); }
    };
    println!("  {} {}", session.drive_id.vendor_id.trim(), session.drive_id.product_id.trim());

    // Wait for disc
    print!("{} ", strings::get("rip.waiting"));
    let _ = stdout().flush();
    match session.wait_ready() {
        Ok(_) => println!("{}", ok),
        Err(e) => { println!("{}", failed); eprintln!("  {}", e); std::process::exit(1); }
    }

    // Init + probe
    print!("{} ", strings::get("rip.initializing"));
    let _ = stdout().flush();
    match session.init() {
        Ok(_) => {
            println!("{}", ok);
            print!("{} ", strings::get("rip.probing"));
            let _ = stdout().flush();
            match session.probe_disc() {
                Ok(_) => println!("{}", ok),
                Err(e) => { println!("{}", failed); eprintln!("  {}", e); }
            }
        }
        Err(e) => {
            println!("{}", failed);
            eprintln!("  {}", strings::fmt("rip.continuing_oem", &[("error", &e.to_string())]));
        }
    }

    // Scan
    print!("{} ", strings::get("rip.scanning"));
    let _ = stdout().flush();
    let opts = match keydb_path {
        Some(ref kp) => libfreemkv::ScanOptions::with_keydb(kp),
        None => libfreemkv::ScanOptions::default(),
    };
    let disc = match libfreemkv::Disc::scan(&mut session, &opts) {
        Ok(d) => { println!("{}", ok); d }
        Err(e) => { println!("{}", failed); eprintln!("  {}", e); std::process::exit(1); }
    };

    // Disc info
    println!();
    println!("  {}: {:.1} GB", strings::get("rip.capacity"), disc.capacity_gb());
    if disc.encrypted {
        if let Some(ref aacs) = disc.aacs {
            println!("  {}", strings::get("rip.aacs_keys_found"));
            println!("  VUK:      {:02x}{:02x}{:02x}{:02x}...",
                aacs.vuk[0], aacs.vuk[1], aacs.vuk[2], aacs.vuk[3]);
            if verbose {
                println!();
                println!("  {}:   {}", strings::get("rip.verbose_aacs_version"), aacs.version);
                println!("  {}:     {:?}", strings::get("rip.verbose_key_source"), aacs.key_source);
                println!("  {}:      {}", strings::get("rip.verbose_disc_hash"), aacs.disc_hash);
                if let Some(v) = aacs.mkb_version {
                    println!("  {}:    {}", strings::get("rip.verbose_mkb_version"), v);
                }
                println!("  {}: {}", strings::get("rip.verbose_bus_encryption"), aacs.bus_encryption);
                println!("  {}:      {}", strings::get("rip.verbose_volume_id"), hex(&aacs.volume_id));
                println!("  {}:      {}", strings::get("rip.verbose_unit_keys"), aacs.unit_keys.len());
                if let Some(ref rdk) = aacs.read_data_key {
                    println!("  {}:  {:02x}{:02x}{:02x}{:02x}...",
                        strings::get("rip.verbose_read_data_key"), rdk[0], rdk[1], rdk[2], rdk[3]);
                }
            }
        } else {
            println!("  {}", strings::get("rip.aacs_no_keys"));
            if verbose {
                println!("  ({})", strings::get("rip.verbose_handshake_hint"));
            }
        }
    }

    // Titles
    println!();
    println!("{} ({}):", strings::get("rip.titles"), disc.titles.len());
    println!();
    let target_idx = title_num.unwrap_or(1).saturating_sub(1);
    for (i, title) in disc.titles.iter().enumerate() {
        let marker = if i == target_idx { ">" } else { " " };
        println!("{} {:2}. {} — {:.1} GB — {}", marker, i + 1,
            title.duration_display(), title.size_gb(), title.playlist);
        for stream in &title.streams {
            match stream {
                libfreemkv::Stream::Video(v) => println!("       {:?} {}", v.codec, v.resolution),
                libfreemkv::Stream::Audio(a) => println!("       {:?} {} {}", a.codec, a.channels, a.language),
                libfreemkv::Stream::Subtitle(s) => println!("       {}", s.language),
            }
        }
    }

    if list_only { return; }
    if disc.encrypted && disc.aacs.is_none() {
        eprintln!("\n{}", strings::get("rip.cannot_rip_no_keys"));
        std::process::exit(1);
    }

    let title = disc.titles[target_idx].clone();

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

    println!();
    println!("{}", strings::fmt("rip.ripping", &[
        ("num", &(target_idx + 1).to_string()),
        ("duration", &title.duration_display()),
        ("size", &format!("{:.1}", title.size_gb())),
        ("file", &out_file.display().to_string()),
    ]));

    if title.extents.is_empty() {
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

    let total_bytes = title.size_bytes;
    let start = std::time::Instant::now();

    // Build the output stream chain
    if raw_mode {
        let mut output = ProgressWriter::new(
            BufWriter::with_capacity(4 * 1024 * 1024, file),
            total_bytes, &INTERRUPTED,
        );
        let mut reader = disc.open_title(&mut session, target_idx).unwrap();
        rip_loop(&mut reader, &mut output);
        let _ = output.inner.flush();
        print_summary(&out_file, start, output.bytes_written, output.peak_speed);
    } else {
        let mkv = libfreemkv::MkvStream::new(BufWriter::with_capacity(4 * 1024 * 1024, file))
            .title(&title)
            .max_buffer(10 * 1024 * 1024);
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
