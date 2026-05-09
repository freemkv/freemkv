// freemkv — subcommand handlers
// AGPL-3.0 — freemkv project
//
// Each module here implements one CLI subcommand. main.rs handles arg
// parsing and dispatches into cmd::*. Shared helpers (output, strings,
// pipe) stay at the crate root.

pub mod disc_info;
pub mod drive_info;
pub mod info;
pub mod update_keys;
pub mod verify;
