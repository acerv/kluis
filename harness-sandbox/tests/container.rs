// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

use hakoniwa::Output;
use harness_sandbox::{restrict_syscalls, NetworkAccess, Sandbox};

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
fn private_folder_is_empty_and_writable() {
    let (dir, file) = folder();
    let sandbox = system().private_dir(dir.path());

    let script = format!(
        "ls -A {0}; stat -c %a {0}; echo x > {0}/new.txt && cat {0}/new.txt",
        dir.path().display()
    );
    let out = run(&sandbox, &script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "700\nx\n");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "data");
    assert!(!dir.path().join("new.txt").exists());
}

#[test]
fn allowed_folders_show_inside_private_folder() {
    let (dir, _) = folder();
    let shown = dir.path().join("shown");
    let hidden = dir.path().join("hidden");
    for sub in [&shown, &hidden] {
        std::fs::create_dir(sub).unwrap();
        std::fs::write(sub.join("f.txt"), "data").unwrap();
    }
    let sandbox = system().private_dir(dir.path()).allow_read(&shown);

    let script = format!(
        "ls -A {}; cat {}/f.txt",
        dir.path().display(),
        shown.display()
    );
    let out = run(&sandbox, &script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "shown\ndata");
}

#[test]
fn private_folder_size_follows_tmp() {
    let (dir, _) = folder();
    let sandbox = system().private_dir(dir.path()).max_tmp_size(1 << 20);

    let script = format!("df --output=size -B1 {} | tail -1", dir.path().display());
    let out = run(&sandbox, &script);
    assert_eq!(stdout(&out).trim(), (1 << 20).to_string());
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
fn unreadable_folder_refuses_the_container() {
    // `chmod 300` hides its `.git` from a scan, not from commands.
    use std::os::unix::fs::PermissionsExt;
    let (dir, _) = repo();
    let mode =
        |mode| std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
    mode(0o300);
    let res = system().allow_write(dir.path()).container();
    mode(0o700);
    let err = res.unwrap_err();
    assert!(err.message.contains("could hide a .git"), "{err}");
}

#[test]
fn git_that_cannot_be_mounted_refuses_the_container() {
    let (dir, _) = folder();
    let git = dir.path().join(OsStr::from_bytes(b"x\xff")).join(".git");
    std::fs::create_dir_all(&git).unwrap();
    let err = system().allow_write(dir.path()).container().unwrap_err();
    assert!(err.message.contains("not UTF-8"), "{err}");
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
fn denied_folder_is_unreadable() {
    let (dir, file) = folder();
    let secret = dir.path().join(".ssh");
    std::fs::create_dir(&secret).unwrap();
    let key = secret.join("id_ed25519");
    std::fs::write(&key, "private-key").unwrap();
    let sandbox = system().allow_read(dir.path()).deny_read(&secret);

    let script = format!(
        "cat {} && (ls {} || true) && (cat {} || true)",
        file.display(),
        secret.display(),
        key.display()
    );
    let out = run(&sandbox, &script);
    assert_eq!(stdout(&out), "data");
    assert!(
        !stdout(&out).contains("private-key") && !stderr(&out).contains("private-key"),
        "{} {}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn denied_file_is_unreadable() {
    let (dir, file) = folder();
    let secret = dir.path().join(".netrc");
    std::fs::write(&secret, "password123").unwrap();
    let sandbox = system().allow_read(dir.path()).deny_read(&secret);

    let script = format!("cat {} && cat {}", file.display(), secret.display());
    let out = run(&sandbox, &script);
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "data");
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn denied_inside_writable_folder() {
    let (dir, _) = folder();
    let sub = dir.path().join("sub");
    let nested = sub.join("nested");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    let secret = sub.join("secret.txt");
    std::fs::write(&secret, "secret").unwrap();
    let sandbox = system()
        .allow_write(dir.path())
        .deny_read(&secret)
        .deny_read(&nested);

    let script = format!(
        "cat {0}; echo evil > {0}; rm -f {0}; ls {1}; \
         mv {2} {3}/old && echo evil > {0}; \
         echo ok > {2}/other.txt",
        secret.display(),
        nested.display(),
        sub.display(),
        dir.path().display()
    );
    let out = run(&sandbox, &script);
    assert!(!stdout(&out).contains("secret"));
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret");
    assert_eq!(
        std::fs::read_to_string(sub.join("other.txt")).unwrap(),
        "ok\n"
    );
}

#[test]
fn denied_overrides_allowed_mount() {
    let (dir, file) = folder();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let sub_file = sub.join("s.txt");
    std::fs::write(&sub_file, "sub-secret").unwrap();

    let sandbox = system()
        .allow_read(dir.path())
        .allow_write(&sub)
        .deny_read(&sub);
    let out = run(&sandbox, &format!("cat {}", sub_file.display()));
    assert!(!out.status.success());
    assert!(!stdout(&out).contains("sub-secret"));

    let sandbox = system().allow_write(dir.path()).deny_read(dir.path());
    let out = run(&sandbox, &format!("cat {}", file.display()));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("No such file"), "{}", stderr(&out));
}

#[test]
fn missing_denied_path_is_ignored() {
    let (_dir, file) = folder();
    let sandbox = system()
        .deny_read("/no/such/secret/folder")
        .deny_read(file.join("under-a-file"));
    let out = run(&sandbox, "echo ok");
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn max_tmp_size_fills_up() {
    let sandbox = system().max_tmp_size(1 << 20);

    let out = run(&sandbox, "head -c 2M /dev/zero > /tmp/big");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("No space left"), "{}", stderr(&out));

    let err = Sandbox::new().max_tmp_size(0).container().unwrap_err();
    assert!(err.message.contains("max_tmp_size"), "{err}");
}

#[test]
fn limit_memory_caps_child_address_space() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let mut cmd = Command::new("/bin/bash");
    cmd.args(["-c", "ulimit -v"]);
    // SAFETY: limit_memory only makes a system call.
    unsafe {
        cmd.pre_exec(|| harness_sandbox::limit_memory(64 << 20));
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", 64 << 10)
    );
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

    let err = Sandbox::new().max_processes(0).container().unwrap_err();
    assert!(err.message.contains("max_processes"), "{err}");
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

#[test]
fn no_network_isolates_container_network() {
    let (_server, port) = server();
    let sandbox = system().no_network();

    let out = run(&sandbox, &connect(port));
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Network is unreachable"),
        "{}",
        stderr(&out)
    );

    let out = run(&sandbox, &format!("echo hi >/dev/udp/127.0.0.1/{port}"));
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Network is unreachable"),
        "{}",
        stderr(&out)
    );

    // Calling `allow_connect` after `no_network` switches to port-filtered mode.
    let out = run(&sandbox.allow_connect(port), &connect(port));
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn own_ipc_and_host_name() {
    // SysV shared memory of host programs must not be reachable, and what
    // commands create must not outlive the container.
    let out = run(
        &system(),
        "readlink /proc/self/ns/ipc; readlink /proc/self/ns/uts",
    );
    let inside = stdout(&out);
    for ns in ["ipc", "uts"] {
        let host = std::fs::read_link(format!("/proc/self/ns/{ns}")).unwrap();
        let host = host.to_str().unwrap();
        assert!(inside.contains(&format!("{ns}:[")), "{inside}");
        assert!(
            !inside.contains(host),
            "{ns} shared with the host: {inside}"
        );
    }
}

#[test]
fn no_core_dumps() {
    // A crash would hand a core file, as commands shape it, to the host's
    // handler, such as systemd-coredump running as root; at one byte the
    // kernel runs none.
    let out = run(
        &system(),
        "grep 'core file' /proc/self/limits; ulimit -c unlimited 2>/dev/null || echo FIXED",
    );
    let text = stdout(&out);
    let limits: Vec<&str> = text.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(limits[4..7], ["1", "1", "bytes"], "{text}");
    assert!(text.contains("FIXED"), "{text}");
}

/// The error of `call` in the container of `sandbox`, its system calls
/// restricted for `network` as the tool executor does, or 0 when it
/// succeeds.
fn call_error<F>(sandbox: &Sandbox, network: NetworkAccess, call: F) -> i32
where
    F: Fn() -> libc::c_long + Send + Sync + 'static,
{
    let container = sandbox.container().unwrap();
    // SAFETY: as in `bind`; the closure only makes plain system calls.
    let mut command = unsafe {
        container.command_from_closure(move || {
            if restrict_syscalls(network).is_err() {
                return -1;
            }
            match call() {
                -1 => std::io::Error::last_os_error().raw_os_error().unwrap(),
                _ => 0,
            }
        })
    };
    command.status().unwrap().code
}

/// Send a byte from a socket pair of `kind` to the datagram socket at
/// `path`.
fn send_from_pair(kind: libc::c_int, path: &Path) -> libc::c_long {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, &src) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
        *dst = src as libc::c_char;
    }
    let mut fds = [0; 2];
    // SAFETY: plain system calls on a buffer and an address that outlive
    // them.
    unsafe {
        if libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) != 0 {
            return -1;
        }
        libc::sendto(
            fds[0],
            b"x".as_ptr().cast(),
            1,
            0,
            (&addr as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        ) as libc::c_long
    }
}

#[test]
fn socket_pairs_do_not_reach_host_sockets() {
    // A datagram socket of the host below a shown folder.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.sock");
    let host = UnixDatagram::bind(&path).unwrap();
    host.set_nonblocking(true).unwrap();
    let sandbox = system().allow_write(dir.path());

    for network in [NetworkAccess::None, NetworkAccess::Ports] {
        for kind in [libc::SOCK_DGRAM, libc::SOCK_STREAM, libc::SOCK_SEQPACKET] {
            let target = path.clone();
            let err = call_error(&sandbox, network, move || send_from_pair(kind, &target));
            if kind == libc::SOCK_DGRAM {
                assert_eq!(err, libc::EACCES, "{network:?}");
            }
            let mut buf = [0];
            let got = host.recv(&mut buf);
            assert!(got.is_err(), "{network:?} {kind}: reached the host");
        }
    }
}
