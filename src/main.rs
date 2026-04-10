// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// AGPL-3.0 — freemkv project

mod info;
mod disc_info;
mod rip;
mod remux;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        usage();
        std::process::exit(0);
    }

    match args[1].as_str() {
        "drive-info" | "info" => info::run(&args[2..]),
        "disc-info" => disc_info::run(&args[2..]),
        "rip" => rip::run(&args[2..]),
        "remux" => remux::run(&args[2..]),
        "update-keys" => update_keys(&args[2..]),
        "version" | "--version" | "-V" => {
            println!("{}", env!("CARGO_PKG_VERSION"));
        }
        "help" | "--help" | "-h" => usage(),
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            eprintln!();
            usage();
            std::process::exit(1);
        }
    }
}

fn usage() {
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage: freemkv <command> [options]");
    println!();
    println!("Commands:");
    println!("  drive-info            Show drive hardware and profile match");
    println!("  disc-info             Show disc titles, streams, and sizes");
    println!("  rip [options]         Back up a disc title");
    println!("  remux <in.m2ts>      Convert m2ts to MKV (no drive needed)");
    println!("  update-keys --url <url>  Download and update KEYDB.cfg");
    println!("  version               Show version");
    println!("  help                  Show this help");
    println!();
    println!("Rip options:");
    println!("  -d, --device /dev/sgN   Specify device (default: auto-detect)");
    println!("  -o, --output /path      Output directory (default: current)");
    println!("  -t, --title N           Title number (default: 1 = main feature)");
    println!("  -k, --keydb /path       Path to KEYDB.cfg for AACS decryption");
    println!("  -l, --list              List titles only, don't rip");
    println!("      --raw               Output raw m2ts instead of MKV");
    println!();
    println!("Examples:");
    println!("  freemkv rip");
    println!("  freemkv rip --title 2 --output ~/Movies/");
    println!("  freemkv rip --raw");
    println!("  freemkv disc-info");
    println!("  freemkv drive-info");
}

fn update_keys(args: &[String]) {
    let mut url: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--url" | "-u" => { i += 1; url = args.get(i).map(|s| s.as_str()); }
            _ => {}
        }
        i += 1;
    }

    let url = match url {
        Some(u) => u,
        None => {
            eprintln!("Usage: freemkv update-keys --url <url>");
            std::process::exit(1);
        }
    };

    match libfreemkv::keydb::update(url) {
        Ok(result) => {
            println!("Updated: {} entries, {} bytes", result.entries, result.bytes);
            println!("Saved: {}", result.path.display());
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
