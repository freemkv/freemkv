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
            println!("freemkv {}", env!("CARGO_PKG_VERSION"));
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
    println!("Usage:");
    println!("  freemkv info                     Show drive information");
    println!("  freemkv info --share             Share profile to help expand drive support");
    println!("  freemkv info --mask              Mask serial numbers in output");
    println!("  freemkv rip [--output /path]     Back up a disc (coming soon)");
    println!("  freemkv version                  Show version");
    println!("  freemkv help                     Show this help");
    println!();
    println!("Options:");
    println!("  --device /dev/sgN                Specify device (default: auto-detect)");
    println!("  --mask                           Mask serial numbers (A=alpha, 0=digit)");
    println!("  --quiet                          Minimal output");
    println!();
    println!("https://github.com/freemkv");
}
