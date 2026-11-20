// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A pseudo-terminal pair: `master` for us, `slave` for the harness.
fn openpty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(ret, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn termios(fd: &OwnedFd) -> libc::termios {
    let mut t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) }, 0);
    t
}

fn is_raw(fd: &OwnedFd) -> bool {
    termios(fd).c_lflag & (libc::ICANON | libc::ECHO) == 0
}

/// Run the TUI on the terminal `slave`, as its controlling terminal.
fn spawn_tui(home: &std::path::Path, slave: &OwnedFd) -> Child {
    let stdio = || Stdio::from(slave.try_clone().unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kl"));
    cmd.arg("--no-sandbox")
        .env("HOME", home)
        .env("TERM", "xterm")
        .env_remove("HARNESS_CONFIG")
        .stdin(stdio())
        .stdout(stdio())
        .stderr(stdio());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().unwrap()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn signals_restore_terminal() {
    for (signal, name) in [
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGHUP, "SIGHUP"),
        (libc::SIGINT, "SIGINT"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let (master, slave) = openpty();
        let mut child = spawn_tui(home.path(), &slave);

        // Keep reading so the harness never blocks on a full terminal.
        let mut output = File::from(master.try_clone().unwrap());
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = output.read_to_end(&mut buf);
        });

        wait_until("raw mode", || is_raw(&slave));
        unsafe { libc::kill(child.id() as libc::pid_t, signal) };
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(128 + signal), "{name}: {status:?}");
        assert_eq!(status.signal(), None, "{name}");
        assert!(!is_raw(&slave), "{name}: terminal left in raw mode");

        drop(slave);
        drop(master);
        let _ = reader.join();
    }
}
