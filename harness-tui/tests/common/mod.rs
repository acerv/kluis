// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Running the TUI on a pseudo-terminal.

#![allow(dead_code)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// A pseudo-terminal pair of `rows` x `cols`: `master` for us, `slave` for
/// the harness.
pub fn openpty(rows: u16, cols: u16) -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let size = winsize(rows, cols);
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(ret, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// Resize the terminal of `master`; its foreground process gets `SIGWINCH`.
pub fn resize(master: &OwnedFd, rows: u16, cols: u16) {
    let size = winsize(rows, cols);
    let ret = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    assert_eq!(ret, 0, "TIOCSWINSZ: {}", std::io::Error::last_os_error());
}

/// Run the TUI without sandbox on the terminal `slave`, as its controlling
/// terminal, with `home` as `$HOME`.
pub fn spawn_tui(home: &Path, slave: &OwnedFd) -> Child {
    spawn_tui_with(home, slave, &[])
}

/// Like [`spawn_tui`], with more arguments.
pub fn spawn_tui_with(home: &Path, slave: &OwnedFd, args: &[&str]) -> Child {
    let stdio = || Stdio::from(slave.try_clone().unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kl"));
    cmd.arg("--no-sandbox")
        .args(args)
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
