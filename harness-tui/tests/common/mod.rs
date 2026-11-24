// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Running the TUI on a pseudo-terminal.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

/// A folder below the target folder: outside `/tmp`, which is private in
/// the container, and outside the current folder, which it shows.
pub fn tempdir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// `kl` with `home` as `$HOME`, away from the config of the user.
pub fn kl(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kl"));
    cmd.env("HOME", home).env_remove("HARNESS_CONFIG");
    cmd
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The headers and the JSON body of the request a mock server got.
pub fn read_request_with_headers(stream: &TcpStream) -> (String, Value) {
    let mut reader = BufReader::new(stream);
    let mut headers = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        headers.push_str(trimmed);
        headers.push('\n');
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    (headers, serde_json::from_slice(&body).unwrap())
}

pub fn read_request(stream: &TcpStream) -> Value {
    read_request_with_headers(stream).1
}

/// Answer a request with `chunks` as an OpenAI stream.
pub fn write_sse(mut stream: TcpStream, chunks: &[Value]) {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).unwrap();
    for chunk in chunks {
        stream
            .write_all(format!("data: {chunk}\n\n").as_bytes())
            .unwrap();
    }
    stream.write_all(b"data: [DONE]\n\n").unwrap();
    stream.flush().unwrap();
}

/// A chunk of an OpenAI stream carrying `delta`.
pub fn delta(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
    })
}

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

/// Wait until `done`, which tells `what` happened.
pub fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Lines of `file`, or 0 when it is missing.
pub fn line_count(file: &Path) -> usize {
    std::fs::read_to_string(file).map_or(0, |text| text.lines().count())
}

/// Send `signal` to `child`.
pub fn kill(child: &Child, signal: i32) {
    unsafe { libc::kill(child.id() as libc::pid_t, signal) };
}

/// Run the TUI without sandbox on the terminal `slave`, as its controlling
/// terminal, with `home` as `$HOME`.
pub fn spawn_tui(home: &Path, slave: &OwnedFd) -> Child {
    spawn_tui_with(home, slave, &[])
}

/// Like [`spawn_tui`], with more arguments.
pub fn spawn_tui_with(home: &Path, slave: &OwnedFd, args: &[&str]) -> Child {
    tui_command(home, slave)
        .arg("--no-sandbox")
        .args(args)
        .spawn()
        .unwrap()
}

/// Like [`spawn_tui`], in the folder `cwd`.
pub fn spawn_tui_in(home: &Path, cwd: &Path, slave: &OwnedFd) -> Child {
    tui_command(home, slave)
        .arg("--no-sandbox")
        .current_dir(cwd)
        .spawn()
        .unwrap()
}

/// Like [`spawn_tui_in`], with the sandbox.
pub fn spawn_sandboxed_tui_in(home: &Path, cwd: &Path, slave: &OwnedFd) -> Child {
    tui_command(home, slave).current_dir(cwd).spawn().unwrap()
}

fn tui_command(home: &Path, slave: &OwnedFd) -> Command {
    let stdio = || Stdio::from(slave.try_clone().unwrap());
    let mut cmd = kl(home);
    cmd.env("TERM", "xterm")
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
    cmd
}
