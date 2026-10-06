use super::*;

// FT11 (§4.3 item 3, SS-21): "`CTRL_C_EVENT` / `CTRL_BREAK_EVENT`: the first sets the flag
// and returns TRUE; the second calls `TerminateProcess(GetCurrentProcess(), 130)`";
// "Close / logoff / shutdown events return FALSE". Per spec; do not change without a cite.
#[test]
fn ctrl_handler_decision() {
    for ev in [CTRL_C_EVENT, CTRL_BREAK_EVENT] {
        assert_eq!(on_ctrl(ev, false), CtrlAction::Handled);
        assert_eq!(on_ctrl(ev, true), CtrlAction::ForceExit(130));
    }
    for ev in [CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT] {
        assert_eq!(on_ctrl(ev, false), CtrlAction::Default);
        assert_eq!(on_ctrl(ev, true), CtrlAction::Default);
    }
}

const CHILD_ENV: &str = "FREEMKV_CLI_STOP_CHILD";

// The child for FT12/FT13: installs the handler, says so, then sits in a call that never
// looks at the token (a stalled key call). Returns at once outside a child run.
#[test]
fn ctrl_c_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    install();
    println!("cli-stop-child-ready");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn child() -> std::process::Command {
    let mut c = std::process::Command::new(std::env::current_exe().expect("test binary"));
    c.args([
        "cli_stop::tests::ctrl_c_child",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD_ENV, "1")
    .stdout(std::process::Stdio::piped());
    c
}

// Block until the child printed its ready line.
fn await_ready(c: &mut std::process::Child) {
    use std::io::BufRead as _;
    let out = c.stdout.take().expect("piped stdout");
    for line in std::io::BufReader::new(out).lines() {
        if line.expect("child stdout").contains("cli-stop-child-ready") {
            return;
        }
    }
    panic!("the child never became ready");
}

// The child's exit status within 10 s, killing it past that.
fn exit_within(mut c: std::process::Child) -> std::process::ExitStatus {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(s) = c.try_wait().expect("try_wait") {
            return s;
        }
        if std::time::Instant::now() >= until {
            let _ = c.kill();
            panic!("the child ignored the second Ctrl-C");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

// Stop design v5 §3.2 (A): "the CLI [updates] on the next tick"; ST-I2's "stopping"
// (§5.7), said once on stderr by the watcher that cancels the process token (GUI = CLI).
#[cfg(unix)]
#[test]
fn unix_first_ctrl_c_says_stopping() {
    use std::io::BufRead as _;
    let mut c = child()
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the child");
    let err = c.stderr.take().expect("piped stderr");
    await_ready(&mut c);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    unsafe { libc::kill(c.id() as libc::pid_t, libc::SIGINT) };
    let stopping = crate::strings::get("stop.stopping");
    let said = std::iter::from_fn(|| rx.recv_timeout(std::time::Duration::from_secs(3)).ok())
        .filter(|l| l.contains(&stopping))
        .count();
    let _ = c.kill();
    let _ = c.wait();
    assert_eq!(said, 1, "'{stopping}' once on stderr");
}

// FT13 / G15 (SS-22): "Unix keeps `_exit(130)`, so the exit code is **130 on both**".
#[cfg(unix)]
#[test]
fn unix_second_ctrl_c_exits_130() {
    let mut c = child().spawn().expect("spawn the child");
    await_ready(&mut c);
    let pid = c.id() as libc::pid_t;
    for _ in 0..2 {
        unsafe { libc::kill(pid, libc::SIGINT) };
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    assert_eq!(exit_within(c).code(), Some(130));
}

// FT12 (§5.5): "exit code 130 while the child is blocked in a stalled key call";
// compile-only on dev, run by qa's Windows release-tests.
#[cfg(windows)]
#[test]
fn windows_second_ctrl_c_exits_130() {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
    }
    let mut c = child()
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .expect("spawn the child");
    await_ready(&mut c);
    for _ in 0..2 {
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, c.id()) };
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    assert_eq!(exit_within(c).code(), Some(130));
}
