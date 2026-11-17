// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output};

use harness_sandbox::{Options, Profile, Sandbox};

/// Run `script` with bash under `sandbox`, applied in the child only.
fn command(sandbox: &Sandbox, script: &str) -> Output {
    let prepared = sandbox.prepare().unwrap();
    let mut cmd = Command::new("bash");
    cmd.args(["-c", script]);
    // SAFETY: restrict_self only makes system calls.
    unsafe {
        cmd.pre_exec(move || prepared.restrict_self());
    }
    cmd.output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run `f` in a new thread restricted by `sandbox`.
fn sandboxed<T: Send + 'static>(sandbox: Sandbox, f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(move || {
        sandbox.apply().unwrap();
        f()
    })
    .join()
    .unwrap()
}

fn server() -> (TcpListener, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[test]
fn commands_run_programs_in_given_folders() {
    let dir = tempfile::tempdir().unwrap();
    let profile = Profile::new(&Options {
        write: vec![dir.path().into()],
        ..Options::default()
    })
    .unwrap();

    let script = format!(
        "cd {} && echo hi > f.txt && cat f.txt && date >/dev/null",
        dir.path().display()
    );
    let out = command(&profile.commands, &script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"hi\n");
}

#[test]
fn commands_cannot_write_elsewhere() {
    let other = tempfile::tempdir().unwrap();
    let profile = Profile::new(&Options::default()).unwrap();

    let out = command(
        &profile.commands,
        &format!("touch {}/f", other.path().display()),
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn private_temporary_folder() {
    let profile = Profile::new(&Options::default()).unwrap();
    let tmp = profile.tmp.path().to_owned();

    let out = command(
        &profile.commands,
        &format!("TMPDIR={} mktemp", tmp.display()),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with(tmp.to_str().unwrap()));

    drop(profile);
    assert!(!tmp.exists());
}

#[test]
fn commands_have_no_network_by_default() {
    let (_server, port) = server();
    let profile = Profile::new(&Options::default()).unwrap();

    let out = command(
        &profile.commands,
        &format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
    );
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn network_option_opens_commands() {
    let (_server, port) = server();
    let profile = Profile::new(&Options {
        network: true,
        ..Options::default()
    })
    .unwrap();

    let out = command(
        &profile.commands,
        &format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
    );
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn harness_connects_to_listed_ports_only() {
    let (_server, port) = server();
    let (_other, other) = server();
    let profile = Profile::new(&Options {
        connect: vec![port],
        ..Options::default()
    })
    .unwrap();

    let (allowed, blocked) = sandboxed(profile.harness, move || {
        (
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
            TcpStream::connect((Ipv4Addr::LOCALHOST, other)),
        )
    });
    assert!(allowed.is_ok());
    assert_eq!(blocked.unwrap_err().kind(), ErrorKind::PermissionDenied);
}

#[test]
fn dns_files_are_for_the_harness_only() {
    let hosts = Path::new("/etc/hosts");
    if !hosts.exists() {
        return;
    }
    let profile = Profile::new(&Options::default()).unwrap();

    let out = command(&profile.commands, "cat /etc/hosts");
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );

    let read = sandboxed(profile.harness, || std::fs::read("/etc/hosts"));
    assert!(read.is_ok());
}

#[test]
fn missing_given_path_fails() {
    let profile = Profile::new(&Options {
        read: vec!["/no/such/folder".into()],
        ..Options::default()
    })
    .unwrap();

    let err = profile.commands.prepare().unwrap_err();
    assert!(err.message.contains("/no/such/folder"), "{err}");
}
