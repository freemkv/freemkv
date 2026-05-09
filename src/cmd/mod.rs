// freemkv — subcommand handlers
// AGPL-3.0 — freemkv project
//
// Each module here implements (or dispatches to) one CLI subcommand.
// main.rs handles arg parsing and dispatches into cmd::*. Shared
// helpers (output, strings, pipe) stay at the crate root.

pub(crate) mod disc_info;
pub(crate) mod drive_info;
pub(crate) mod info;
pub(crate) mod update_keys;
pub(crate) mod verify;
