// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;

use hakoniwa::Output;
use harness_sandbox::{Options, Sandbox};

/// Run `script` with bash in the default container for `options`.
fn run(options: &Options, script: &str) -> Output {
    Sandbox::with_defaults(options)
        .container()
        .unwrap()
        .command("/bin/bash")
        .args(["-c", script])
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn server() -> (TcpListener, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn connect(port: u16) -> String {
    format!("exec 3<>/dev/tcp/127.0.0.1/{port}")
}

#[test]
fn programs_run_in_given_folders() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        write: vec![dir.path().into()],
        ..Options::default()
    };

    let script = format!(
        "cd {} && echo hi > f.txt && cat f.txt && date >/dev/null",
        dir.path().display()
    );
    let out = run(&options, &script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"hi\n");
}

#[test]
fn dns_files_are_readable() {
    if !Path::new("/etc/hosts").exists() {
        return;
    }
    let out = run(&Options::default(), "cat /etc/hosts");
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn cannot_write_elsewhere() {
    let out = run(&Options::default(), "touch /f /usr/f");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert_eq!(err.matches("Read-only file system").count(), 2, "{err}");
}

#[test]
fn connect_to_listed_ports_only() {
    let (_server, port) = server();
    let (_other, other) = server();
    let options = Options {
        connect: vec![port],
        ..Options::default()
    };

    let out = run(&options, &connect(port));
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run(&options, &connect(other));
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn network_option_allows_any_port() {
    let (_server, port) = server();
    let options = Options {
        network: true,
        ..Options::default()
    };

    let out = run(&options, &connect(port));
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn home_is_read_only() {
    let home = std::env::var("HOME").unwrap();
    let script = format!("ls -A {home} | wc -l; touch {home}/.harness-test");
    let out = run(&Options::default(), &script);

    let entries = std::fs::read_dir(&home).unwrap().count();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        entries.to_string()
    );
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn processes_are_limited() {
    let out = run(&Options::default(), "ulimit -u");
    assert_eq!(out.stdout, b"1024\n");
}

#[test]
fn tmp_is_shared_with_host() {
    let file = std::env::temp_dir().join(format!("harness-test-{}", std::process::id()));
    let out = run(&Options::default(), &format!("echo x > {}", file.display()));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x\n");
    std::fs::remove_file(&file).unwrap();
}

#[test]
fn missing_given_path_fails() {
    let options = Options {
        read: vec!["/no/such/folder".into()],
        ..Options::default()
    };
    let err = Sandbox::with_defaults(&options).container().unwrap_err();
    assert!(err.message.contains("/no/such/folder"), "{err}");
}
