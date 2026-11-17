// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use harness_sandbox::Sandbox;

/// Run `f` in a new thread restricted by `sandbox`; Landlock applies to the
/// calling thread only, so other tests are unaffected.
fn sandboxed<T: Send + 'static>(sandbox: Sandbox, f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(move || {
        sandbox.apply().unwrap();
        f()
    })
    .join()
    .unwrap()
}

/// A temporary folder holding `file.txt`.
fn folder() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.txt");
    std::fs::write(&file, "data").unwrap();
    (dir, file)
}

fn denied<T: std::fmt::Debug>(r: std::io::Result<T>) -> bool {
    r.unwrap_err().kind() == ErrorKind::PermissionDenied
}

/// System folders needed to run programs.
fn system(mut sandbox: Sandbox) -> Sandbox {
    for path in ["/usr", "/bin", "/lib", "/lib64", "/etc"] {
        if Path::new(path).exists() {
            sandbox = sandbox.allow_read(path);
        }
    }
    sandbox
}

#[test]
fn read_only_folder() {
    let (dir, file) = folder();
    let new = dir.path().join("new.txt");
    let sandbox = Sandbox::new().allow_read(dir.path());

    let (read, write, create) = sandboxed(sandbox, move || {
        (
            std::fs::read_to_string(&file),
            std::fs::write(&file, "changed"),
            std::fs::write(&new, "x"),
        )
    });
    assert_eq!(read.unwrap(), "data");
    assert!(denied(write));
    assert!(denied(create));
}

#[test]
fn writable_folder() {
    let (dir, file) = folder();
    let new = dir.path().join("sub/new.txt");
    let sandbox = Sandbox::new().allow_write(dir.path());

    let result = sandboxed(sandbox, move || {
        std::fs::write(&file, "changed")?;
        std::fs::create_dir(new.parent().unwrap())?;
        std::fs::write(&new, "x")?;
        std::fs::read_to_string(&file)
    });
    assert_eq!(result.unwrap(), "changed");
}

#[test]
fn single_file_rule() {
    let (dir, file) = folder();
    let other = dir.path().join("other.txt");
    std::fs::write(&other, "other").unwrap();
    let sandbox = Sandbox::new().allow_read(&file);

    let (allowed, sibling) = sandboxed(sandbox, move || {
        (
            std::fs::read_to_string(&file),
            std::fs::read_to_string(&other),
        )
    });
    assert_eq!(allowed.unwrap(), "data");
    assert!(denied(sibling));
}

#[test]
fn other_folders_denied() {
    let (allowed, _) = folder();
    let (_outside, file) = folder();
    let sandbox = Sandbox::new().allow_write(allowed.path());

    let read = sandboxed(sandbox, move || std::fs::read_to_string(&file));
    assert!(denied(read));
}

#[test]
fn child_processes_inherit() {
    let (allowed, inside) = folder();
    let (_outside, outside) = folder();
    let sandbox = system(Sandbox::new()).allow_write(allowed.path());

    let (ok, blocked) = sandboxed(sandbox, move || {
        let cat = |f: &Path| std::process::Command::new("cat").arg(f).output().unwrap();
        (cat(&inside), cat(&outside))
    });
    assert_eq!(ok.stdout, b"data");
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("Permission denied"));
}

#[test]
fn other_threads_unaffected() {
    let (allowed, _) = folder();
    let (_outside, file) = folder();
    let sandbox = Sandbox::new().allow_write(allowed.path());

    let path = file.clone();
    assert!(denied(sandboxed(sandbox, move || std::fs::read(&path))));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "data");
}

#[test]
fn dev_null_always_allowed() {
    let result = sandboxed(Sandbox::new(), || {
        std::fs::write("/dev/null", "x")?;
        std::fs::read("/dev/null")
    });
    assert!(result.unwrap().is_empty());
}

#[test]
fn missing_path_fails() {
    let err = std::thread::spawn(|| {
        Sandbox::new()
            .allow_read("/no/such/folder")
            .apply()
            .unwrap_err()
    })
    .join()
    .unwrap();
    assert!(err.message.contains("/no/such/folder"), "{err}");
}

/// A local TCP server and its port.
fn server() -> (TcpListener, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[test]
fn connect_denied_by_default() {
    let (_server, port) = server();

    let r = sandboxed(Sandbox::new(), move || {
        TcpStream::connect((Ipv4Addr::LOCALHOST, port))
    });
    assert!(denied(r));
}

#[test]
fn connect_to_allowed_port() {
    let (_server, port) = server();
    let (_other, other) = server();
    let sandbox = Sandbox::new().allow_connect(port);

    let (allowed, blocked) = sandboxed(sandbox, move || {
        (
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
            TcpStream::connect((Ipv4Addr::LOCALHOST, other)),
        )
    });
    assert!(allowed.is_ok());
    assert!(denied(blocked));
}

#[test]
fn bind_only_allowed_port() {
    // Free ports: bind, note the number, release.
    let (free, port) = server();
    let (other_free, other) = server();
    drop((free, other_free));
    let sandbox = Sandbox::new().allow_bind(port);

    let (allowed, blocked) = sandboxed(sandbox, move || {
        (
            TcpListener::bind((Ipv4Addr::LOCALHOST, port)),
            TcpListener::bind((Ipv4Addr::LOCALHOST, other)),
        )
    });
    assert!(allowed.is_ok());
    assert!(denied(blocked));
}

#[test]
fn any_network_keeps_file_rules() {
    let (_server, port) = server();
    let (_outside, file) = folder();
    let sandbox = Sandbox::new().allow_connect(1).allow_any_network();

    let (connect, bind, read) = sandboxed(sandbox, move || {
        (
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)),
            std::fs::read(&file),
        )
    });
    assert!(connect.is_ok());
    assert!(bind.is_ok());
    assert!(denied(read));
}

#[test]
fn prepared_sandbox_restricts_only_the_child() {
    let (_server, port) = server();
    let (dir, file) = folder();
    let prepared = system(Sandbox::new())
        .allow_read(dir.path())
        .prepare()
        .unwrap();

    // Allowed folder, then a TCP connection through bash's /dev/tcp.
    let script = format!("cat {}; exec 3<>/dev/tcp/127.0.0.1/{port}", file.display());
    let mut cmd = std::process::Command::new("bash");
    cmd.args(["-c", &script]);
    // SAFETY: restrict_self only makes system calls.
    unsafe {
        cmd.pre_exec(move || prepared.restrict_self());
    }
    let out = cmd.output().unwrap();

    assert_eq!(out.stdout, b"data");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Permission denied"));
    // The parent is still free.
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());
}
