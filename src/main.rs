// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// AGPL-3.0 — freemkv project

mod info;
mod disc_info;
mod rip;
mod scsi;
mod strings;

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
    println!("  rip [--output /path]  Back up a disc (coming soon)");
    println!("  version               Show version");
    println!("  help                  Show this help");
    println!();
    println!("Global options:");
    println!("  --device /dev/sgN     Specify device (default: auto-detect)");
    println!("  --quiet               Minimal output");
    println!();
    println!("Drive-info options:");
    println!("  --share               Share profile to help expand drive support");
    println!("  --mask                Mask serial numbers (use with --share)");
    println!();
    println!("Examples:");
    println!("  freemkv drive-info");
    println!("  freemkv drive-info --share --mask");
    println!("  freemkv disc-info");
    println!("  freemkv rip --output ~/backups/");
}
