// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// AGPL-3.0 — freemkv project

mod info;
mod disc_info;
mod rip;
mod remux;
mod strings;
mod output;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Parse --language before init so strings load in the right locale.
    // Strip --language/--lang + value from args so they don't reach subcommands.
    let mut filtered_args = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if (args[i] == "--language" || args[i] == "--lang") && i + 1 < args.len() {
            strings::set_language(&args[i + 1]);
            i += 2;
        } else {
            filtered_args.push(args[i].clone());
            i += 1;
        }
    }
    let args = filtered_args;
    strings::init();

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
            eprintln!("{}", strings::fmt("app.unknown_command", &[("cmd", &args[1])]));
            eprintln!();
            usage();
            std::process::exit(1);
        }
    }
}

fn usage() {
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("{}", strings::get("app.usage"));
    println!();
    println!("{}:", strings::get("app.commands"));
    println!("  drive-info            {}", strings::get("app.cmd_drive_info"));
    println!("  disc-info             {}", strings::get("app.cmd_disc_info"));
    println!("  rip [options]         {}", strings::get("app.cmd_rip"));
    println!("  remux <in.m2ts>      {}", strings::get("app.cmd_remux"));
    println!("  update-keys --url     {}", strings::get("app.cmd_update_keys"));
    println!("  version               {}", strings::get("app.cmd_version"));
    println!("  help                  {}", strings::get("app.cmd_help"));
    println!();
    println!("{}:", strings::get("app.rip_options"));
    println!("  -d, --device /dev/sgN   {}", strings::get("app.opt_device"));
    println!("  -o, --output /path      {}", strings::get("app.opt_output"));
    println!("  -t, --title N           {}", strings::get("app.opt_title"));
    println!("  -k, --keydb /path       {}", strings::get("app.opt_keydb"));
    println!("  -l, --list              {}", strings::get("app.opt_list"));
    println!("      --raw               {}", strings::get("app.opt_raw"));
    println!();
    println!("{}:", strings::get("app.drive_info_options"));
    println!("  -s, --share             {}", strings::get("app.opt_share"));
    println!("  -m, --mask              {}", strings::get("app.opt_mask"));
    println!();
    println!("{}:", strings::get("app.global_options"));
    println!("  -q, --quiet             {}", strings::get("app.opt_quiet"));
    println!("  -v, --verbose           {}", strings::get("app.opt_verbose"));
    println!();
    println!("{}:", strings::get("app.examples"));
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
            eprintln!("{}", strings::get("keys.usage"));
            std::process::exit(1);
        }
    };

    match libfreemkv::keydb::update(url) {
        Ok(result) => {
            println!("{}", strings::fmt("keys.updated", &[
                ("entries", &result.entries.to_string()),
                ("bytes", &result.bytes.to_string()),
            ]));
            println!("{}", strings::fmt("keys.saved", &[("path", &result.path.display().to_string())]));
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}
