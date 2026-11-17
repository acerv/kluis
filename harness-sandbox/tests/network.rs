// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output};

use harness_sandbox::NoNetwork;

/// Run `script` with bash without network.
fn run(script: &str) -> Output {
    let no_network = NoNetwork::new();
    let mut cmd = Command::new("bash");
    cmd.args(["-c", script]);
    // SAFETY: apply only makes system calls.
    unsafe {
        cmd.pre_exec(move || no_network.apply());
    }
    cmd.output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn tcp_is_unreachable() {
    let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = server.local_addr().unwrap().port();

    let out = run(&format!("exec 3<>/dev/tcp/127.0.0.1/{port}"));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("unreachable"), "{}", stderr(&out));
    // The parent keeps its network.
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());
}

#[test]
fn udp_is_unreachable() {
    let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = server.local_addr().unwrap().port();

    let out = run(&format!("echo x > /dev/udp/127.0.0.1/{port}"));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("unreachable"), "{}", stderr(&out));
}

#[test]
fn keeps_user_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    let uid = std::fs::metadata(dir.path()).unwrap().uid();

    let out = run(&format!("id -u && touch {}", file.display()));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), uid.to_string());
    assert_eq!(std::fs::metadata(&file).unwrap().uid(), uid);
}
