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
    for path in ["/etc/hosts", "/etc/os-release", "/etc/services"] {
        if Path::new(path).exists() {
            let out = run(&Options::default(), &format!("cat {path}"));
            assert!(out.status.success(), "{path}: {}", stderr(&out));
        }
    }
    if Path::new("/etc/alternatives").exists() {
        let out = run(&Options::default(), "ls /etc/alternatives");
        assert!(out.status.success(), "{}", stderr(&out));
    }
}

#[test]
fn cannot_write_elsewhere() {
    let out = run(&Options::default(), "touch /f /usr/f");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert_eq!(err.matches("Read-only file system").count(), 2, "{err}");
}

#[test]
fn default_isolates_all_network() {
    let (_server, port) = server();
    let out = run(&Options::default(), &connect(port));
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Network is unreachable"),
        "{}",
        stderr(&out)
    );
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

    let options = Options {
        max_processes: Some(256),
        ..Options::default()
    };
    let out = run(&options, "ulimit -u");
    assert_eq!(out.stdout, b"256\n");
}

#[test]
fn tmp_size_is_configurable() {
    let options = Options {
        tmp_size: Some(1 << 20),
        ..Options::default()
    };
    let out = run(&options, "df --output=size -B1 /tmp | tail -1");
    let size: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert_eq!(size, 1 << 20);
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

#[test]
fn denied_paths_are_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join(".ssh");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("id_ed25519"), "secret-key").unwrap();
    let public = dir.path().join("public.txt");
    std::fs::write(&public, "public\n").unwrap();

    let options = Options {
        read: vec![dir.path().into()],
        deny: vec![secret.clone(), dir.path().join("missing")],
        ..Options::default()
    };
    let script = format!("cat {} && ls {}", public.display(), secret.display());
    let out = run(&options, &script);
    assert!(!out.status.success());
    assert_eq!(out.stdout, b"public\n");
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}
