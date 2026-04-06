// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// AGPL-3.0 — freemkv project

mod info;
mod scsi;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        usage();
        std::process::exit(0);
    }

    match args[1].as_str() {
        "info" => info::run(&args[2..]),
        "rip" => {
            eprintln!("freemkv rip: not yet implemented");
            eprintln!();
            eprintln!("Track progress at https://github.com/freemkv/freemkv");
            std::process::exit(1);
        }
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
    println!("  info                Show drive information");
    println!("  rip [--output /path]  Back up a disc (coming soon)");
    println!("  version             Show version");
    println!("  help                Show this help");
    println!();
    println!("Global options:");
    println!("  --device /dev/sgN   Specify device (default: auto-detect)");
    println!("  --quiet             Minimal output");
    println!();
    println!("Info options:");
    println!("  --share             Share your drive profile to help expand drive support");
    println!("  --mask              Mask serial numbers (use with --share)");
    println!();
    println!("Examples:");
    println!("  freemkv info");
    println!("  freemkv info --share");
    println!("  freemkv info --share --mask");
}
