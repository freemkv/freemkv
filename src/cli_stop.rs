//! The CLI's Ctrl-C (stop design v5 §4.3, "CLI Ctrl-C (v5.2)"): one handler, one watcher and
//! one process-wide `Halt` that every CLI op takes. The second Ctrl-C exits 130 on Unix and
//! Windows alike; close, logoff and shutdown keep the system's default handling.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the first Ctrl-C (SIGINT, or a console Ctrl-C / Ctrl-Break).
pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Console control events (`HandlerRoutine`'s `dwCtrlType`), SS-21.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const CTRL_C_EVENT: u32 = 0;
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const CTRL_BREAK_EVENT: u32 = 1;
/// Close, logoff and shutdown: [`on_ctrl`] leaves them to the system default.
#[cfg(test)]
pub(crate) const CTRL_CLOSE_EVENT: u32 = 2;
#[cfg(test)]
pub(crate) const CTRL_LOGOFF_EVENT: u32 = 5;
#[cfg(test)]
pub(crate) const CTRL_SHUTDOWN_EVENT: u32 = 6;

/// What the console handler does with one control event (§4.3 item 3).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CtrlAction {
    /// Set the flag and return TRUE.
    Handled,
    /// `TerminateProcess(GetCurrentProcess(), code)`.
    ForceExit(u32),
    /// Return FALSE: the next handler (the system default) runs.
    Default,
}

/// The console handler's decision, pure so every OS tests it (FT11).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn on_ctrl(event: u32, already_interrupted: bool) -> CtrlAction {
    // SS-21 `HandlerRoutine`: "If the function handles the control signal, it should return
    // TRUE. If it returns FALSE, the next handler function in the list … is used."
    match event {
        CTRL_C_EVENT | CTRL_BREAK_EVENT if already_interrupted => CtrlAction::ForceExit(130),
        CTRL_C_EVENT | CTRL_BREAK_EVENT => CtrlAction::Handled,
        _ => CtrlAction::Default,
    }
}

/// The process token every CLI op takes (§4.3 item 1): cancelled within one
/// [`WAIT_SLICE`](libfreemkv::halt::WAIT_SLICE) of the first Ctrl-C once [`install`] ran.
pub(crate) fn token() -> &'static libfreemkv::Halt {
    static TOKEN: std::sync::OnceLock<libfreemkv::Halt> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| watch(&INTERRUPTED, true))
}

/// Install the Ctrl-C handler and the process token's watcher, once per process.
pub(crate) fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        install_handler();
        token();
    });
}

fn install_handler() {
    #[cfg(unix)]
    unsafe {
        // sigaction, not signal(): on musl, signal() is one-shot and would
        // kill the double-Ctrl-C _exit(130) guard after the first fire.
        // SA_RESTART (no SA_RESETHAND) fixes that and restarts syscalls.
        let mut sa: libc::sigaction = std::mem::zeroed();
        // Cast through a thin pointer: a bare `fn as usize` is a double
        // coercion that clippy 1.97 rejects.
        sa.sa_sigaction = handle_sigint as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        // On failure, degrade gracefully: the handler simply isn't installed.
        let _ = libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }

    #[cfg(windows)]
    unsafe {
        extern "system" fn handler(event: u32) -> i32 {
            match on_ctrl(event, INTERRUPTED.load(Ordering::SeqCst)) {
                CtrlAction::Handled => {
                    INTERRUPTED.store(true, Ordering::SeqCst);
                    1
                }
                // SS-21: "TerminateProcess … is used to unconditionally cause a process to
                // exit", not ExitProcess: "the DLL detach code … results in a deadlock".
                // SAFETY: the current-process pseudo-handle is always valid; no Rust state is read.
                CtrlAction::ForceExit(code) => unsafe {
                    TerminateProcess(GetCurrentProcess(), code);
                    1
                },
                CtrlAction::Default => 0,
            }
        }
        unsafe extern "system" {
            fn SetConsoleCtrlHandler(
                handler: unsafe extern "system" fn(u32) -> i32,
                add: i32,
            ) -> i32;
            fn GetCurrentProcess() -> isize;
            fn TerminateProcess(process: isize, exit_code: u32) -> i32;
        }
        SetConsoleCtrlHandler(handler, 1);
    }
}

#[cfg(unix)]
extern "C" fn handle_sigint(_sig: libc::c_int) {
    if INTERRUPTED.load(Ordering::SeqCst) {
        unsafe { libc::_exit(130) };
    }
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// A `Halt` cancelled within one `WAIT_SLICE` of `flag` being set: the test hook (§4.3, "`watching(flag)` survives as
/// the test hook"). Its watcher ends once `flag` is set or the returned token is dropped.
#[cfg(test)]
pub(crate) fn watching(flag: &'static AtomicBool) -> libfreemkv::Halt {
    watch(flag, false)
}

// The watcher; `say`: tell the user once, on stderr, that the Stop was taken (§3.2 (A)).
fn watch(flag: &'static AtomicBool, say: bool) -> libfreemkv::Halt {
    let halt = libfreemkv::Halt::new();
    let watched = halt.clone();
    std::thread::spawn(move || {
        while std::sync::Arc::strong_count(watched.as_arc()) > 1 {
            if flag.load(Ordering::Acquire) {
                watched.cancel();
                if say {
                    eprintln!("{}", crate::strings::get_or("stop.stopping", "Stopping …"));
                }
                return;
            }
            std::thread::sleep(libfreemkv::halt::WAIT_SLICE);
        }
    });
    halt
}

#[cfg(test)]
#[path = "cli_stop_tests.rs"]
mod tests;
