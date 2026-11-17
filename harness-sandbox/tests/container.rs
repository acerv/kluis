// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};

use hakoniwa::Output;
use harness_sandbox::Sandbox;

/// Run `script` with bash in the container of `sandbox`.
fn run(sandbox: &Sandbox, script: &str) -> Output {
    sandbox
        .container()
        .unwrap()
        .command("/bin/bash")
        .args(["-c", script])
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// System folders needed to run programs.
fn system() -> Sandbox {
    let mut sandbox = Sandbox::new();
    for path in ["/usr", "/bin", "/lib", "/lib64"] {
        if Path::new(path).exists() {
            sandbox = sandbox.allow_read(path);
        }
    }
    sandbox
}

/// A temporary folder holding `file.txt`.
fn folder() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.txt");
    std::fs::write(&file, "data").unwrap();
    (dir, file)
}

/// A local TCP server and its port.
fn server() -> (TcpListener, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// A port nobody listens on.
fn free_port() -> u16 {
    server().1
}

fn connect(port: u16) -> String {
    format!("exec 3<>/dev/tcp/127.0.0.1/{port}")
}

/// Bind a TCP socket to `port` inside the container of `sandbox`:
/// 0 on success, 1 when denied, 2 on other errors.
fn bind(sandbox: &Sandbox, port: u16) -> i32 {
    let container = sandbox.container().unwrap();
    // SAFETY: the closure runs in the forked child, which hakoniwa already
    // sets up with allocations of its own; it only binds a socket.
    let mut command = unsafe {
        container.command_from_closure(move || {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                Ok(_) => 0,
                Err(e) if e.kind() == ErrorKind::PermissionDenied => 1,
                Err(_) => 2,
            }
        })
    };
    command.status().unwrap().code
}

#[test]
fn read_only_folder() {
    let (dir, file) = folder();
    let sandbox = system().allow_read(dir.path());

    let script = format!("cat {0}; echo x > {0}", file.display());
    let out = run(&sandbox, &script);
    assert_eq!(stdout(&out), "data");
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "data");
}

#[test]
fn writable_folder() {
    let (dir, file) = folder();
    let sandbox = system().allow_write(dir.path());

    let script = format!(
        "echo changed > {} && mkdir {1}/sub && echo x > {1}/sub/new.txt",
        file.display(),
        dir.path().display()
    );
    let out = run(&sandbox, &script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed\n");
    assert!(dir.path().join("sub/new.txt").exists());
}

#[test]
fn single_file_mount() {
    let (dir, file) = folder();
    let other = dir.path().join("other.txt");
    std::fs::write(&other, "other").unwrap();
    let sandbox = system().allow_read(&file);

    let out = run(&sandbox, &format!("cat {}", file.display()));
    assert_eq!(stdout(&out), "data");
    let out = run(&sandbox, &format!("cat {}", other.display()));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("No such file"), "{}", stderr(&out));
}

#[test]
fn other_folders_hidden() {
    let (allowed, _) = folder();
    let (_outside, file) = folder();
    let sandbox = system().allow_write(allowed.path());

    let out = run(&sandbox, &format!("cat {}", file.display()));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("No such file"), "{}", stderr(&out));
}

#[test]
fn write_inside_read_only_folder() {
    let (dir, file) = folder();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let sandbox = system().allow_read(dir.path()).allow_write(&sub);

    let script = format!(
        "echo x > {}/new.txt && echo y > {}",
        sub.display(),
        file.display()
    );
    let out = run(&sandbox, &script);
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert!(sub.join("new.txt").exists());
}

/// A writable folder holding a `.git` folder with `config`.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let (dir, _) = folder();
    let git = dir.path().join(".git");
    std::fs::create_dir_all(git.join("hooks")).unwrap();
    std::fs::write(git.join("config"), "").unwrap();
    (dir, git)
}

#[test]
fn git_folder_read_only() {
    let (dir, git) = repo();
    let sandbox = system().allow_write(dir.path());

    let script = format!(
        "cat {0}/config && echo x > {1}/file.txt && echo y > {0}/hooks/pre-commit",
        git.display(),
        dir.path().display()
    );
    let out = run(&sandbox, &script);
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "x\n"
    );
    assert!(!git.join("hooks/pre-commit").exists());
}

#[test]
fn git_file_read_only() {
    // Worktrees and submodules point to their git folder with a file.
    let (dir, _) = folder();
    let git = dir.path().join(".git");
    std::fs::write(&git, "gitdir: /elsewhere\n").unwrap();
    let sandbox = system().allow_write(dir.path());

    let out = run(
        &sandbox,
        &format!("echo 'gitdir: /evil' > {}", git.display()),
    );
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(&git).unwrap(),
        "gitdir: /elsewhere\n"
    );
}

#[test]
fn git_folder_writable_when_allowed() {
    let (dir, git) = repo();
    let sandbox = system().allow_write(dir.path()).allow_write(&git);

    let out = run(&sandbox, &format!("echo x > {}/config", git.display()));
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn max_processes_counts_container_only() {
    let container = Sandbox::new().max_processes(16).container().unwrap();
    // SAFETY: as in `bind`; the closure only forks children that sleep.
    let mut command = unsafe {
        container.command_from_closure(|| {
            let mut started = 0;
            for _ in 0..32 {
                match libc::fork() {
                    0 => {
                        libc::sleep(2);
                        libc::_exit(0);
                    }
                    pid if pid > 0 => started += 1,
                    _ => {}
                }
            }
            started
        })
    };
    let started = command.status().unwrap().code;
    // Some start: the processes of the user outside do not count.
    assert!(started > 0 && started < 16, "{started}");
}

#[test]
fn missing_path_fails() {
    let err = Sandbox::new()
        .allow_read("/no/such/folder")
        .container()
        .unwrap_err();
    assert!(err.message.contains("/no/such/folder"), "{err}");
}

#[test]
fn private_tmp() {
    let name = format!("harness-test-{}", std::process::id());
    let out = run(
        &system(),
        &format!("echo x > /tmp/{name} && cat /tmp/{name}"),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!Path::new("/tmp").join(&name).exists());
}

#[test]
fn dev_null_works() {
    let out = run(&system(), "echo x > /dev/null && cat /dev/null");
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn connect_denied_by_default() {
    let (_server, port) = server();
    let out = run(&system(), &connect(port));
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn connect_to_allowed_port() {
    let (_server, port) = server();
    let (_other, other) = server();
    let sandbox = system().allow_connect(port);

    let out = run(&sandbox, &connect(port));
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run(&sandbox, &connect(other));
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn any_network_allows_connect() {
    let (_server, port) = server();
    let out = run(&system().allow_any_network(), &connect(port));
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn bind_needs_any_network() {
    let port = free_port();
    assert_eq!(bind(&Sandbox::new().allow_connect(port), port), 1);
    assert_eq!(bind(&Sandbox::new().allow_any_network(), port), 0);
}
