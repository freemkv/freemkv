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
mod style;

use crate::strings as s;

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
        "version" | "--version" | "-V" => println!(
            "{}",
            style::dim(&format!("freemkv {}", env!("CARGO_PKG_VERSION")))
        ),
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
                eprintln!("{}", s::get("app.usage_brief"));
                eprintln!();
                eprintln!("{}", s::get("app.help_hint"));
                std::process::exit(1);
            }
        }
    }
}

fn usage() {
    // Body lives in `app.help_text` per locale (en.json is the source of
    // truth; other locales fall back to English content until a
    // translator refines them — the strings test only verifies key
    // presence + placeholder consistency, not value content). The
    // version banner is dim() and prepended in code so we don't need a
    // version-substituted locale key.
    println!(
        "{}",
        style::dim(&format!("freemkv {}", env!("CARGO_PKG_VERSION")))
    );
    println!();
    print!("{}", s::get("app.help_text"));
}
