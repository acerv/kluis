// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::ffi::OsStrExt;
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
fn kept_read_only_inside_writable_folder() {
    let (dir, file) = folder();
    let sandbox = system().allow_write(dir.path()).keep_read_only(&file);

    let script = format!(
        "cat {0}; echo x > {1}/new.txt; echo y > {0}; rm -f {0}; mv {0} {1}/moved",
        file.display(),
        dir.path().display()
    );
    let out = run(&sandbox, &script);
    assert_eq!(stdout(&out), "data");
    assert!(dir.path().join("new.txt").exists());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "data");
}

#[test]
fn kept_read_only_survives_renamed_parent() {
    // Moving the folder above away would leave the path free for a new file.
    let (dir, _) = folder();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let file = sub.join("config.toml");
    std::fs::write(&file, "data").unwrap();
    let sandbox = system().allow_write(dir.path()).keep_read_only(&file);

    let script = format!(
        "echo x > {0}/other.txt; mv {0} {1}/old; mkdir -p {0} && echo evil > {2}",
        sub.display(),
        dir.path().display(),
        file.display()
    );
    run(&sandbox, &script);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "data");
    // The rest of the folder stays writable.
    assert!(sub.join("other.txt").exists());
}

#[test]
fn kept_read_only_wins_over_writable() {
    let (_dir, file) = folder();
    let sandbox = system().allow_write(&file).keep_read_only(&file);

    run(&sandbox, &format!("echo y > {}", file.display()));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "data");
}

#[test]
fn kept_read_only_is_readable() {
    let (_dir, file) = folder();
    let sandbox = system().keep_read_only(&file);

    let out = run(&sandbox, &format!("cat {}", file.display()));
    assert_eq!(stdout(&out), "data");
}

#[test]
fn nested_git_folder_read_only() {
    let (dir, _) = folder();
    let nested = dir.path().join("sub/nested");
    let git = nested.join(".git");
    std::fs::create_dir_all(git.join("hooks")).unwrap();
    std::fs::write(git.join("config"), "cfg\n").unwrap();
    let sandbox = system().allow_write(dir.path());

    let script = format!(
        "cat {0}/config && echo x > {0}/hooks/pre-commit; \
         mv {1} {2}/old && mkdir -p {1}/.git/hooks && echo evil > {0}/hooks/pre-commit; \
         echo ok > {1}/code.txt",
        git.display(),
        nested.display(),
        dir.path().display()
    );
    let out = run(&sandbox, &script);
    assert_eq!(stdout(&out), "cfg\n");
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(nested.join("code.txt")).unwrap(),
        "ok\n"
    );
    assert!(!git.join("hooks/pre-commit").exists());
}

#[test]
fn nested_git_file_read_only() {
    let (dir, _) = folder();
    let nested = dir.path().join("sub/mod");
    std::fs::create_dir_all(&nested).unwrap();
    let git = nested.join(".git");
    std::fs::write(&git, "gitdir: /elsewhere\n").unwrap();
    let sandbox = system().allow_write(dir.path());

    let script = format!(
        "echo evil > {0}; mv {1} {2}/old && echo evil > {0}",
        git.display(),
        dir.path().join("sub").display(),
        dir.path().display()
    );
    run(&sandbox, &script);
    assert_eq!(
        std::fs::read_to_string(&git).unwrap(),
        "gitdir: /elsewhere\n"
    );
}

#[test]
fn nested_git_writable_when_allowed() {
    let (dir, _) = folder();
    let git_allowed = dir.path().join("a/.git");
    let git_kept = dir.path().join("b/.git");
    for git in [&git_allowed, &git_kept] {
        std::fs::create_dir_all(git).unwrap();
        std::fs::write(git.join("config"), "init\n").unwrap();
    }
    let sandbox = system().allow_write(dir.path()).allow_write(&git_allowed);

    let out = run(
        &sandbox,
        &format!(
            "echo ok > {}/config && echo no > {}/config",
            git_allowed.display(),
            git_kept.display()
        ),
    );
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read_to_string(git_allowed.join("config")).unwrap(),
        "ok\n"
    );
    assert_eq!(
        std::fs::read_to_string(git_kept.join("config")).unwrap(),
        "init\n"
    );
}

#[test]
fn kept_read_only_protects_symlink_in_writable_subfolder() {
    let (dir, _) = folder();
    let (_outside, real) = folder();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let link = sub.join("link.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let sandbox = system().allow_write(dir.path()).keep_read_only(&link);

    let script = format!(
        "cat {0}; echo ok > {1}/ok.txt; rm -f {0}; echo evil > {0}",
        link.display(),
        dir.path().display()
    );
    let out = run(&sandbox, &script);
    assert_eq!(stdout(&out), "data");
    assert!(dir.path().join("ok.txt").exists());
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "data");
}

#[test]
fn kept_read_only_protects_dir_symlink_in_writable_subfolder() {
    let (dir, _) = folder();
    let (outside, real) = folder();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let dirlink = sub.join("dirlink");
    std::os::unix::fs::symlink(outside.path(), &dirlink).unwrap();
    let target = dirlink.join("file.txt");
    let sandbox = system().allow_write(dir.path()).keep_read_only(&target);

    let script = format!(
        "rm -f {0}; echo evil > {1}; mkdir -p {0} && echo evil > {2}",
        dirlink.display(),
        real.display(),
        target.display()
    );
    run(&sandbox, &script);
    assert!(std::fs::symlink_metadata(&dirlink)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "data");
}

#[test]
fn kept_read_only_under_symlinked_writable_root() {
    let (base, _) = folder();
    let real_root = base.path().join("real");
    let sub = real_root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let secret = sub.join("secret.txt");
    std::fs::write(&secret, "secret").unwrap();
    let link_root = base.path().join("link");
    std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

    let via_link = link_root.join("sub/secret.txt");
    let sandbox = system().allow_write(&link_root).keep_read_only(&via_link);

    let script = format!(
        "echo ok > {0}/sub/other.txt; echo evil > {1}; mv {0}/sub {0}/old && echo evil > {1}",
        link_root.display(),
        via_link.display()
    );
    let out = run(&sandbox, &script);
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert!(sub.join("other.txt").exists());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret");
}

#[test]
fn kept_read_only_parent_covers_child_pin() {
    let (dir, _) = folder();
    let a = dir.path().join("a");
    let b = a.join("b");
    std::fs::create_dir_all(&b).unwrap();
    let file = b.join("file.txt");
    std::fs::write(&file, "data").unwrap();
    // Child registered before parent: parent read-only must still win for `b`.
    let sandbox = system()
        .allow_write(dir.path())
        .keep_read_only(&file)
        .keep_read_only(&a);

    let out = run(&sandbox, &format!("echo evil > {}/other.txt", b.display()));
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert!(!b.join("other.txt").exists());
}

#[test]
fn kept_read_only_inside_read_only_system_folder() {
    // A root-owned file inside `/usr` (and reachable via `/bin` symlink)
    // must not be bind-mounted again on top of `/usr`, which would fail
    // trying to touch the host file.
    let sandbox = system().keep_read_only("/bin/sh");
    let out = run(&sandbox, "test -r /bin/sh");
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn nested_git_under_symlinked_writable_root_does_not_expose_canonical() {
    let (base, _) = folder();
    let real_root = base.path().join("real");
    let git = real_root.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("config"), "cfg\n").unwrap();
    let link_root = base.path().join("link");
    std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

    let sandbox = system().allow_write(&link_root);
    let out = run(
        &sandbox,
        &format!(
            "echo evil > {}/.git/config; test -e {}",
            link_root.display(),
            git.display()
        ),
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(git.join("config")).unwrap(),
        "cfg\n"
    );
}

#[test]
fn non_utf8_nested_git_does_not_fail_startup() {
    let (dir, _) = folder();
    let bad = dir
        .path()
        .join(OsStr::from_bytes(b"bad\xffdir"))
        .join(".git");
    std::fs::create_dir_all(&bad).unwrap();

    let sandbox = system().allow_write(dir.path());
    let out = run(&sandbox, "echo ok");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "ok\n");
}

#[test]
fn keep_read_only_rejects_non_utf8_symlink_parent() {
    let (dir, _) = folder();
    let (_outside, real) = folder();
    let bad = dir.path().join(OsStr::from_bytes(b"bad\xffdir"));
    let sub = dir.path().join("sub");
    std::fs::create_dir(&bad).unwrap();
    std::fs::create_dir(&sub).unwrap();
    let hop = bad.join("hop");
    std::os::unix::fs::symlink(&real, &hop).unwrap();
    let link = sub.join("link");
    std::os::unix::fs::symlink(&hop, &link).unwrap();

    let err = system()
        .allow_write(dir.path())
        .keep_read_only(&link)
        .container()
        .unwrap_err();
    assert!(err.message.contains("not UTF-8"), "{err}");
}

#[test]
fn allow_write_overrides_allow_read_on_same_path() {
    let (dir, file) = folder();
    let keep = dir.path().join("keep.txt");
    std::fs::write(&keep, "keep").unwrap();
    let sandbox = system()
        .allow_read(dir.path())
        .allow_write(dir.path())
        .allow_write(dir.path())
        .keep_read_only(&keep);

    let out = run(
        &sandbox,
        &format!(
            "echo ok > {} && echo no > {}",
            file.display(),
            keep.display()
        ),
    );
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "ok\n");
    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "keep");
}

#[test]
fn kept_read_only_overrides_writable_child() {
    let (dir, _) = folder();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let child = sub.join("child.txt");
    std::fs::write(&child, "data").unwrap();
    let sandbox = system().allow_write(&sub).keep_read_only(dir.path());

    let out = run(&sandbox, &format!("echo evil > {}", child.display()));
    assert!(
        stderr(&out).contains("Read-only file system"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read_to_string(&child).unwrap(), "data");
}

#[test]
fn max_tmp_size_fills_up() {
    let sandbox = system().max_tmp_size(1 << 20);

    let out = run(&sandbox, "head -c 2M /dev/zero > /tmp/big");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("No space left"), "{}", stderr(&out));
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
