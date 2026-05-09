// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// AGPL-3.0 — freemkv project
//
// Usage: freemkv <source> <dest> [flags]
//        freemkv info <url> [flags]
//
// Examples:
//   freemkv disc:// mkv://Dune.mkv
//   freemkv disc:///dev/sg4 m2ts://Dune.m2ts
//   freemkv m2ts://Dune.m2ts mkv://Dune.mkv
//   freemkv disc:// network://10.1.7.11:9000
//   freemkv info disc://

mod cmd;
mod output;
mod pipe;
mod strings;

fn main() {
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt::init();
    }
    let args: Vec<String> = std::env::args().collect();

    // Parse --language before anything else
    let mut filtered = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if (args[i] == "--language" || args[i] == "--lang") && i + 1 < args.len() {
            strings::set_language(&args[i + 1]);
            i += 2;
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }
    let args = filtered;
    strings::init();

    if args.len() < 2 {
        usage();
        std::process::exit(0);
    }

    match args[1].as_str() {
        "info" => cmd::info::run(&args[2..]),
        "verify" => cmd::verify::run(&args[2..]),
        "update-keys" => cmd::update_keys::run(&args[2..]),
        "version" | "--version" | "-V" => println!("{}", env!("CARGO_PKG_VERSION")),
        "help" | "--help" | "-h" => usage(),

        // Everything else: freemkv <source> <dest>
        _ => {
            // Flags that consume the next argument as a value
            const VALUE_FLAGS: &[&str] = &["-t", "--title", "-k", "--keydb"];

            // Collect URLs (non-flag args) and flags
            let mut urls = Vec::new();
            let mut flags = Vec::new();
            let mut skip_next = false;
            for arg in &args[1..] {
                if skip_next {
                    skip_next = false;
                    continue;
                }
                if arg.starts_with('-') {
                    flags.push(arg.clone());
                    if VALUE_FLAGS.contains(&arg.as_str()) {
                        skip_next = true;
                    }
                } else {
                    urls.push(arg.clone());
                }
            }

            if urls.len() == 2 {
                if !pipe::run(&urls[0], &urls[1], &args[1..]) {
                    std::process::exit(1);
                }
            } else if urls.len() == 1 {
                // Single URL with no dest — show info
                cmd::info::run(&args[1..]);
            } else {
                eprintln!("Usage: freemkv <source> <dest> [flags]");
                eprintln!("       freemkv info <url>");
                eprintln!();
                eprintln!("Try 'freemkv help' for more.");
                std::process::exit(1);
            }
        }
    }
}

fn usage() {
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage: freemkv <source> <dest> [flags]");
    println!("       freemkv info <url> [flags]");
    println!("       freemkv verify [disc://]");
    println!();
    println!("Stream URLs:");
    println!("  disc://                  Optical drive (auto-detect)");
    println!("  disc:///dev/sg4          Optical drive (Linux)");
    println!("  disc://D:                Optical drive (Windows)");
    println!("  mkv://path.mkv           Matroska file");
    println!("  m2ts://path.m2ts         BD transport stream file");
    println!("  network://host:port      TCP stream");
    println!("  stdio://                 Stdin/stdout pipe");
    println!("  iso://image.iso          Blu-ray ISO image");
    println!("  null://                  Discard (benchmarking)");
    println!();
    println!("  All URLs require a scheme:// prefix.");
    println!("  File paths follow the scheme: mkv://./Dune.mkv, m2ts:///tmp/Dune.m2ts");
    println!();
    println!("Examples:");
    println!("  freemkv disc:// mkv://Dune.mkv                     Rip disc to MKV");
    println!("  freemkv disc:// m2ts://Dune.m2ts                   Rip disc to m2ts");
    println!("  freemkv disc:///dev/sg4 mkv://Dune.mkv             Rip specific drive");
    println!("  freemkv disc:// mkv://Movie.mkv                    Rip all titles");
    println!("  freemkv disc:// mkv://Movie.mkv -t 1               Rip main feature only");
    println!("  freemkv disc:// mkv://Movie.mkv -t 1 -t 3          Rip titles 1 and 3");
    println!(
        "  freemkv disc:// iso://Disc.iso                     Full disc to ISO (auto-resumes)"
    );
    println!(
        "  freemkv disc:// iso://Disc.iso --raw               Full disc, no decryption (auto-resumes)"
    );
    println!(
        "  freemkv disc:// iso://Disc.iso --multipass        Sweep with mapfile for multipass recovery"
    );
    println!(
        "  freemkv iso://Disc.iso iso://Disc.iso --multipass Patch bad sectors (one retry pass)"
    );
    println!("  freemkv iso://Disc.iso mkv://Movie.mkv             ISO to MKV");
    println!("  freemkv m2ts://Movie.m2ts mkv://Movie.mkv          Remux m2ts to MKV");
    println!("  freemkv disc:// network://10.1.7.11:9000           Stream to network");
    println!("  freemkv network://0.0.0.0:9000 mkv://Movie.mkv    Receive from network");
    println!("  freemkv disc:// stdio://                           Pipe to stdout");
    println!("  freemkv disc:// null://                            Benchmark read speed");
    println!("  freemkv info disc://                               Show disc info");
    println!();
    println!("Flags:");
    println!("  -t, --title N       Select title (1-based, repeatable). Default: all.");
    println!("  -k, --keydb PATH    KEYDB.cfg path");
    println!("  -v, --verbose       Show AACS/drive debug info");
    println!("  -q, --quiet         Suppress output");
    println!("      --raw           Skip decryption (raw encrypted output)");
    println!("      --multipass    Write/update mapfile for multipass recovery");
    println!("  -s, --share         Submit drive profile (with info disc://)");
    println!("  -m, --mask          Mask serial numbers (with --share)");
}
