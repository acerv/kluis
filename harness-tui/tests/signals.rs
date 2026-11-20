// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::time::{Duration, Instant};

use common::{openpty, spawn_tui};

fn is_raw(fd: &OwnedFd) -> bool {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) }, 0);
    t.c_lflag & (libc::ICANON | libc::ECHO) == 0
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
        let (master, slave) = openpty(24, 80);
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
