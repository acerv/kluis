// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

use common::{connect, errno, fs_size, inside, list, read, system, write};
use harness_sandbox::{restrict_syscalls, NetworkAccess, Sandbox};
use libc::{EACCES, ENETUNREACH, ENOENT, ENOSPC, EROFS};

/// A temporary folder holding `file.txt`.
fn folder() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.txt");
    fs::write(&file, "data").unwrap();
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

#[test]
fn read_only_folder() {
    let (dir, file) = folder();
    let sandbox = system().allow_read(dir.path());

    assert_eq!(inside(&sandbox, read(&file)).unwrap(), "data");
    assert_eq!(errno(inside(&sandbox, write(&file, "x"))), EROFS);
    assert_eq!(fs::read_to_string(&file).unwrap(), "data");
}

#[test]
fn writable_folder() {
    let (dir, file) = folder();
    let sandbox = system().allow_write(dir.path());

    let (f, sub) = (file.clone(), dir.path().join("sub"));
    inside(&sandbox, move || {
        fs::write(&f, "changed")?;
        fs::create_dir(&sub)?;
        fs::write(sub.join("new.txt"), "x")?;
        Ok(String::new())
    })
    .unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "changed");
    assert!(dir.path().join("sub/new.txt").exists());
}

#[test]
fn single_file_mount() {
    let (dir, file) = folder();
    let other = dir.path().join("other.txt");
    fs::write(&other, "other").unwrap();
    let sandbox = system().allow_read(&file);

    assert_eq!(inside(&sandbox, read(&file)).unwrap(), "data");
    assert_eq!(errno(inside(&sandbox, read(&other))), ENOENT);
}

#[test]
fn other_folders_hidden() {
    let (allowed, _) = folder();
    let (_outside, file) = folder();
    let sandbox = system().allow_write(allowed.path());

    assert_eq!(errno(inside(&sandbox, read(&file))), ENOENT);
}

#[test]
fn private_folder_is_empty_and_writable() {
    let (dir, file) = folder();
    let sandbox = system().private_dir(dir.path());

    let path = dir.path().to_path_buf();
    let out = inside(&sandbox, move || {
        let names = list(&path)()?;
        let mode = fs::metadata(&path)?.permissions().mode() & 0o777;
        fs::write(path.join("new.txt"), "x")?;
        Ok(format!(
            "{names}{mode:o} {}",
            fs::read_to_string(path.join("new.txt"))?
        ))
    });
    assert_eq!(out.unwrap(), "700 x");
    assert_eq!(fs::read_to_string(&file).unwrap(), "data");
    assert!(!dir.path().join("new.txt").exists());
}

#[test]
fn allowed_folders_show_inside_private_folder() {
    let (dir, _) = folder();
    let shown = dir.path().join("shown");
    let hidden = dir.path().join("hidden");
    for sub in [&shown, &hidden] {
        fs::create_dir(sub).unwrap();
        fs::write(sub.join("f.txt"), "data").unwrap();
    }
    let sandbox = system().private_dir(dir.path()).allow_read(&shown);

    assert_eq!(inside(&sandbox, list(dir.path())).unwrap(), "shown\n");
    assert_eq!(
        inside(&sandbox, read(&shown.join("f.txt"))).unwrap(),
        "data"
    );
}

#[test]
fn private_folder_size_follows_tmp() {
    let (dir, _) = folder();
    let sandbox = system().private_dir(dir.path()).max_tmp_size(1 << 20);

    let size = inside(&sandbox, fs_size(dir.path())).unwrap();
    assert_eq!(size, (1 << 20).to_string());
}

#[test]
fn write_inside_read_only_folder() {
    let (dir, file) = folder();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let sandbox = system().allow_read(dir.path()).allow_write(&sub);

    inside(&sandbox, write(&sub.join("new.txt"), "x")).unwrap();
    assert_eq!(errno(inside(&sandbox, write(&file, "y"))), EROFS);
    assert!(sub.join("new.txt").exists());
}

/// A writable folder holding a `.git` folder with `config`.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let (dir, _) = folder();
    let git = dir.path().join(".git");
    fs::create_dir_all(git.join("hooks")).unwrap();
    fs::write(git.join("config"), "").unwrap();
    (dir, git)
}

#[test]
fn git_folder_read_only() {
    let (dir, git) = repo();
    let sandbox = system().allow_write(dir.path());

    inside(&sandbox, read(&git.join("config"))).unwrap();
    inside(&sandbox, write(&dir.path().join("file.txt"), "x")).unwrap();
    let hook = git.join("hooks/pre-commit");
    assert_eq!(errno(inside(&sandbox, write(&hook, "y"))), EROFS);
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "x"
    );
    assert!(!hook.exists());
}

#[test]
fn git_file_read_only() {
    // Worktrees and submodules point to their git folder with a file.
    let (dir, _) = folder();
    let git = dir.path().join(".git");
    fs::write(&git, "gitdir: /elsewhere\n").unwrap();
    let sandbox = system().allow_write(dir.path());

    assert_eq!(errno(inside(&sandbox, write(&git, "gitdir: /evil"))), EROFS);
    assert_eq!(fs::read_to_string(&git).unwrap(), "gitdir: /elsewhere\n");
}

#[test]
fn git_folder_writable_when_allowed() {
    let (dir, git) = repo();
    let sandbox = system().allow_write(dir.path()).allow_write(&git);

    inside(&sandbox, write(&git.join("config"), "x")).unwrap();
}

#[test]
fn unreadable_folder_refuses_the_container() {
    // `chmod 300` hides its `.git` from a scan, not from commands.
    let (dir, _) = repo();
    let mode = |mode| fs::set_permissions(dir.path(), fs::Permissions::from_mode(mode)).unwrap();
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
    fs::create_dir_all(&git).unwrap();
    let err = system().allow_write(dir.path()).container().unwrap_err();
    assert!(err.message.contains("not UTF-8"), "{err}");
}

#[test]
fn kept_read_only_inside_writable_folder() {
    let (dir, file) = folder();
    let sandbox = system().allow_write(dir.path()).keep_read_only(&file);

    let (f, d) = (file.clone(), dir.path().to_path_buf());
    let out = inside(&sandbox, move || {
        let data = fs::read_to_string(&f)?;
        fs::write(d.join("new.txt"), "x")?;
        let _ = fs::write(&f, "y");
        let _ = fs::remove_file(&f);
        let _ = fs::rename(&f, d.join("moved"));
        Ok(data)
    });
    assert_eq!(out.unwrap(), "data");
    assert!(dir.path().join("new.txt").exists());
    assert_eq!(fs::read_to_string(&file).unwrap(), "data");
}

#[test]
fn kept_read_only_survives_renamed_parent() {
    // Moving the folder above away would leave the path free for a new file.
    let (dir, _) = folder();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let file = sub.join("config.toml");
    fs::write(&file, "data").unwrap();
    let sandbox = system().allow_write(dir.path()).keep_read_only(&file);

    let (s, d, f) = (sub.clone(), dir.path().to_path_buf(), file.clone());
    inside(&sandbox, move || {
        fs::write(s.join("other.txt"), "x")?;
        let _ = fs::rename(&s, d.join("old"));
        let _ = fs::create_dir_all(&s);
        let _ = fs::write(&f, "evil");
        Ok(String::new())
    })
    .unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "data");
    // The rest of the folder stays writable.
    assert!(sub.join("other.txt").exists());
}

#[test]
fn kept_read_only_wins_over_writable() {
    let (_dir, file) = folder();
    let sandbox = system().allow_write(&file).keep_read_only(&file);

    assert_eq!(errno(inside(&sandbox, write(&file, "y"))), EROFS);
    assert_eq!(fs::read_to_string(&file).unwrap(), "data");
}

#[test]
fn kept_read_only_is_readable() {
    let (_dir, file) = folder();
    let sandbox = system().keep_read_only(&file);

    assert_eq!(inside(&sandbox, read(&file)).unwrap(), "data");
}

#[test]
fn nested_git_folder_read_only() {
    let (dir, _) = folder();
    let nested = dir.path().join("sub/nested");
    let git = nested.join(".git");
    fs::create_dir_all(git.join("hooks")).unwrap();
    fs::write(git.join("config"), "cfg\n").unwrap();
    let sandbox = system().allow_write(dir.path());

    let hook = git.join("hooks/pre-commit");
    assert_eq!(
        inside(&sandbox, read(&git.join("config"))).unwrap(),
        "cfg\n"
    );
    assert_eq!(errno(inside(&sandbox, write(&hook, "x"))), EROFS);

    let (n, d, h) = (nested.clone(), dir.path().to_path_buf(), hook.clone());
    inside(&sandbox, move || {
        let _ = fs::rename(&n, d.join("old"));
        let _ = fs::create_dir_all(n.join(".git/hooks"));
        let _ = fs::write(&h, "evil");
        fs::write(n.join("code.txt"), "ok")?;
        Ok(String::new())
    })
    .unwrap();
    assert_eq!(fs::read_to_string(nested.join("code.txt")).unwrap(), "ok");
    assert!(!hook.exists());
}

#[test]
fn nested_git_file_read_only() {
    let (dir, _) = folder();
    let nested = dir.path().join("sub/mod");
    fs::create_dir_all(&nested).unwrap();
    let git = nested.join(".git");
    fs::write(&git, "gitdir: /elsewhere\n").unwrap();
    let sandbox = system().allow_write(dir.path());

    assert_eq!(errno(inside(&sandbox, write(&git, "evil"))), EROFS);
    let (g, d) = (git.clone(), dir.path().to_path_buf());
    inside(&sandbox, move || {
        let _ = fs::rename(d.join("sub"), d.join("old"));
        let _ = fs::write(&g, "evil");
        Ok(String::new())
    })
    .unwrap();
    assert_eq!(fs::read_to_string(&git).unwrap(), "gitdir: /elsewhere\n");
}

#[test]
fn nested_git_writable_when_allowed() {
    let (dir, _) = folder();
    let git_allowed = dir.path().join("a/.git");
    let git_kept = dir.path().join("b/.git");
    for git in [&git_allowed, &git_kept] {
        fs::create_dir_all(git).unwrap();
        fs::write(git.join("config"), "init\n").unwrap();
    }
    let sandbox = system().allow_write(dir.path()).allow_write(&git_allowed);

    inside(&sandbox, write(&git_allowed.join("config"), "ok")).unwrap();
    let res = inside(&sandbox, write(&git_kept.join("config"), "no"));
    assert_eq!(errno(res), EROFS);
    assert_eq!(
        fs::read_to_string(git_allowed.join("config")).unwrap(),
        "ok"
    );
    assert_eq!(
        fs::read_to_string(git_kept.join("config")).unwrap(),
        "init\n"
    );
}

#[test]
fn kept_read_only_protects_symlink_in_writable_subfolder() {
    let (dir, _) = folder();
    let (_outside, real) = folder();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let link = sub.join("link.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let sandbox = system().allow_write(dir.path()).keep_read_only(&link);

    let (l, d) = (link.clone(), dir.path().to_path_buf());
    let out = inside(&sandbox, move || {
        let data = fs::read_to_string(&l)?;
        fs::write(d.join("ok.txt"), "ok")?;
        let _ = fs::remove_file(&l);
        let _ = fs::write(&l, "evil");
        Ok(data)
    });
    assert_eq!(out.unwrap(), "data");
    assert!(dir.path().join("ok.txt").exists());
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&real).unwrap(), "data");
}

#[test]
fn kept_read_only_protects_dir_symlink_in_writable_subfolder() {
    let (dir, _) = folder();
    let (outside, real) = folder();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let dirlink = sub.join("dirlink");
    std::os::unix::fs::symlink(outside.path(), &dirlink).unwrap();
    let target = dirlink.join("file.txt");
    let sandbox = system().allow_write(dir.path()).keep_read_only(&target);

    let (l, r, t) = (dirlink.clone(), real.clone(), target);
    inside(&sandbox, move || {
        let _ = fs::remove_file(&l);
        let _ = fs::write(&r, "evil");
        let _ = fs::create_dir_all(&l);
        let _ = fs::write(&t, "evil");
        Ok(String::new())
    })
    .unwrap();
    assert!(fs::symlink_metadata(&dirlink)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&real).unwrap(), "data");
}

#[test]
fn kept_read_only_under_symlinked_writable_root() {
    let (base, _) = folder();
    let real_root = base.path().join("real");
    let sub = real_root.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let secret = sub.join("secret.txt");
    fs::write(&secret, "secret").unwrap();
    let link_root = base.path().join("link");
    std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

    let via_link = link_root.join("sub/secret.txt");
    let sandbox = system().allow_write(&link_root).keep_read_only(&via_link);

    inside(&sandbox, write(&link_root.join("sub/other.txt"), "ok")).unwrap();
    assert_eq!(errno(inside(&sandbox, write(&via_link, "evil"))), EROFS);
    let (root, v) = (link_root.clone(), via_link.clone());
    inside(&sandbox, move || {
        let _ = fs::rename(root.join("sub"), root.join("old"));
        let _ = fs::write(&v, "evil");
        Ok(String::new())
    })
    .unwrap();
    assert!(sub.join("other.txt").exists());
    assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
}

#[test]
fn kept_read_only_parent_covers_child_pin() {
    let (dir, _) = folder();
    let a = dir.path().join("a");
    let b = a.join("b");
    fs::create_dir_all(&b).unwrap();
    let file = b.join("file.txt");
    fs::write(&file, "data").unwrap();
    // Child registered before parent: parent read-only must still win for `b`.
    let sandbox = system()
        .allow_write(dir.path())
        .keep_read_only(&file)
        .keep_read_only(&a);

    let other = b.join("other.txt");
    assert_eq!(errno(inside(&sandbox, write(&other, "evil"))), EROFS);
    assert!(!other.exists());
}

#[test]
fn kept_read_only_inside_read_only_system_folder() {
    // A root-owned file inside `/usr` (and reachable via `/bin` symlink)
    // must not be bind-mounted again on top of `/usr`, which would fail
    // trying to touch the host file.
    let sandbox = system().keep_read_only("/bin/sh");
    let res = inside(&sandbox, || {
        fs::File::open("/bin/sh").map(|_| String::new())
    });
    res.unwrap();
}

#[test]
fn nested_git_under_symlinked_writable_root_does_not_expose_canonical() {
    let (base, _) = folder();
    let real_root = base.path().join("real");
    let git = real_root.join(".git");
    fs::create_dir_all(&git).unwrap();
    fs::write(git.join("config"), "cfg\n").unwrap();
    let link_root = base.path().join("link");
    std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

    let sandbox = system().allow_write(&link_root);
    let res = inside(&sandbox, write(&link_root.join(".git/config"), "evil"));
    assert_eq!(errno(res), EROFS);
    assert_eq!(errno(inside(&sandbox, list(&git))), ENOENT);
    assert_eq!(fs::read_to_string(git.join("config")).unwrap(), "cfg\n");
}

#[test]
fn keep_read_only_rejects_non_utf8_symlink_parent() {
    let (dir, _) = folder();
    let (_outside, real) = folder();
    let bad = dir.path().join(OsStr::from_bytes(b"bad\xffdir"));
    let sub = dir.path().join("sub");
    fs::create_dir(&bad).unwrap();
    fs::create_dir(&sub).unwrap();
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
    fs::write(&keep, "keep").unwrap();
    let sandbox = system()
        .allow_read(dir.path())
        .allow_write(dir.path())
        .allow_write(dir.path())
        .keep_read_only(&keep);

    inside(&sandbox, write(&file, "ok")).unwrap();
    assert_eq!(errno(inside(&sandbox, write(&keep, "no"))), EROFS);
    assert_eq!(fs::read_to_string(&file).unwrap(), "ok");
    assert_eq!(fs::read_to_string(&keep).unwrap(), "keep");
}

#[test]
fn kept_read_only_overrides_writable_child() {
    let (dir, _) = folder();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let child = sub.join("child.txt");
    fs::write(&child, "data").unwrap();
    let sandbox = system().allow_write(&sub).keep_read_only(dir.path());

    assert_eq!(errno(inside(&sandbox, write(&child, "evil"))), EROFS);
    assert_eq!(fs::read_to_string(&child).unwrap(), "data");
}

#[test]
fn denied_folder_is_unreadable() {
    let (dir, file) = folder();
    let secret = dir.path().join(".ssh");
    fs::create_dir(&secret).unwrap();
    let key = secret.join("id_ed25519");
    fs::write(&key, "private-key").unwrap();
    let sandbox = system().allow_read(dir.path()).deny_read(&secret);

    assert_eq!(inside(&sandbox, read(&file)).unwrap(), "data");
    assert_eq!(errno(inside(&sandbox, list(&secret))), EACCES);
    assert_eq!(errno(inside(&sandbox, read(&key))), EACCES);
}

#[test]
fn denied_file_is_unreadable() {
    let (dir, file) = folder();
    let secret = dir.path().join(".netrc");
    fs::write(&secret, "password123").unwrap();
    let sandbox = system().allow_read(dir.path()).deny_read(&secret);

    assert_eq!(inside(&sandbox, read(&file)).unwrap(), "data");
    assert_eq!(errno(inside(&sandbox, read(&secret))), EACCES);
}

#[test]
fn denied_inside_writable_folder() {
    let (dir, _) = folder();
    let sub = dir.path().join("sub");
    let nested = sub.join("nested");
    fs::create_dir_all(nested.join(".git")).unwrap();
    let secret = sub.join("secret.txt");
    fs::write(&secret, "secret").unwrap();
    let sandbox = system()
        .allow_write(dir.path())
        .deny_read(&secret)
        .deny_read(&nested);

    assert_eq!(errno(inside(&sandbox, read(&secret))), EACCES);
    assert_eq!(errno(inside(&sandbox, list(&nested))), EACCES);
    let (f, s, d) = (secret.clone(), sub.clone(), dir.path().to_path_buf());
    inside(&sandbox, move || {
        let _ = fs::write(&f, "evil");
        let _ = fs::remove_file(&f);
        let _ = fs::rename(&s, d.join("old"));
        let _ = fs::write(&f, "evil");
        fs::write(s.join("other.txt"), "ok")?;
        Ok(String::new())
    })
    .unwrap();
    assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
    assert_eq!(fs::read_to_string(sub.join("other.txt")).unwrap(), "ok");
}

#[test]
fn denied_overrides_allowed_mount() {
    let (dir, file) = folder();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let sub_file = sub.join("s.txt");
    fs::write(&sub_file, "sub-secret").unwrap();

    let sandbox = system()
        .allow_read(dir.path())
        .allow_write(&sub)
        .deny_read(&sub);
    assert_eq!(errno(inside(&sandbox, read(&sub_file))), EACCES);

    let sandbox = system().allow_write(dir.path()).deny_read(dir.path());
    assert_eq!(errno(inside(&sandbox, read(&file))), ENOENT);
}

#[test]
fn missing_denied_path_is_ignored() {
    let (_dir, file) = folder();
    let sandbox = system()
        .deny_read("/no/such/secret/folder")
        .deny_read(file.join("under-a-file"));
    inside(&sandbox, || Ok(String::new())).unwrap();
}

#[test]
fn max_tmp_size_fills_up() {
    let sandbox = system().max_tmp_size(1 << 20);

    let res = inside(&sandbox, || {
        fs::write("/tmp/big", vec![0; 2 << 20]).map(|()| String::new())
    });
    assert_eq!(errno(res), ENOSPC);

    let err = Sandbox::new().max_tmp_size(0).container().unwrap_err();
    assert!(err.message.contains("max_tmp_size"), "{err}");
}

#[test]
fn limit_memory_caps_child_address_space() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let mut cmd = Command::new("cat");
    cmd.arg("/proc/self/limits");
    // SAFETY: limit_memory only makes a system call.
    unsafe {
        cmd.pre_exec(|| harness_sandbox::limit_memory(64 << 20));
    }
    let out = cmd.output().unwrap();
    let limits = String::from_utf8(out.stdout).unwrap();
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max address space"))
        .unwrap();
    let soft = line.split_whitespace().nth(3).unwrap();
    assert_eq!(soft, (64 << 20).to_string(), "{line}");
}

#[test]
fn max_processes_counts_container_only() {
    let sandbox = Sandbox::new().max_processes(16);
    let started = inside(&sandbox, || {
        let mut started = 0;
        for _ in 0..32 {
            // SAFETY: the children only sleep.
            match unsafe { libc::fork() } {
                0 => unsafe {
                    libc::sleep(2);
                    libc::_exit(0);
                },
                pid if pid > 0 => started += 1,
                _ => {}
            }
        }
        Ok(started.to_string())
    });
    let started: u32 = started.unwrap().parse().unwrap();
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
    let file = Path::new("/tmp").join(format!("harness-test-{}", std::process::id()));
    let f = file.clone();
    let out = inside(&system(), move || {
        fs::write(&f, "x")?;
        fs::read_to_string(&f)
    });
    assert_eq!(out.unwrap(), "x");
    assert!(!file.exists());
}

#[test]
fn dev_null_works() {
    let sandbox = system();
    inside(&sandbox, write(Path::new("/dev/null"), "x")).unwrap();
    assert_eq!(inside(&sandbox, read(Path::new("/dev/null"))).unwrap(), "");
}

#[test]
fn connect_denied_by_default() {
    let (_server, port) = server();
    assert_eq!(errno(inside(&system(), connect(port))), EACCES);
}

#[test]
fn connect_to_allowed_port() {
    let (_server, port) = server();
    let (_other, other) = server();
    let sandbox = system().allow_connect(port);

    inside(&sandbox, connect(port)).unwrap();
    assert_eq!(errno(inside(&sandbox, connect(other))), EACCES);
}

#[test]
fn any_network_allows_connect() {
    let (_server, port) = server();
    inside(&system().allow_any_network(), connect(port)).unwrap();
}

#[test]
fn bind_needs_any_network() {
    let port = free_port();
    let bind = move || TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map(|_| String::new());
    let res = inside(&Sandbox::new().allow_connect(port), bind);
    assert_eq!(errno(res), EACCES);
    inside(&Sandbox::new().allow_any_network(), bind).unwrap();
}

#[test]
fn no_network_isolates_container_network() {
    let (_server, port) = server();
    let sandbox = system().no_network();

    assert_eq!(errno(inside(&sandbox, connect(port))), ENETUNREACH);
    let res = inside(&sandbox, move || {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.send_to(b"hi", (Ipv4Addr::LOCALHOST, port))?;
        Ok(String::new())
    });
    assert_eq!(errno(res), ENETUNREACH);

    // Calling `allow_connect` after `no_network` switches to port-filtered mode.
    inside(&sandbox.allow_connect(port), connect(port)).unwrap();
}

#[test]
fn own_ipc_and_host_name() {
    // SysV shared memory of host programs must not be reachable, and what
    // commands create must not outlive the container.
    for ns in ["ipc", "uts"] {
        let path = format!("/proc/self/ns/{ns}");
        let host = fs::read_link(&path).unwrap();
        let seen = inside(&system(), move || {
            Ok(fs::read_link(&path)?.to_string_lossy().into_owned())
        })
        .unwrap();
        assert!(seen.starts_with(&format!("{ns}:[")), "{seen}");
        assert_ne!(Path::new(&seen), host, "{ns} shared with the host");
    }
}

#[test]
fn no_core_dumps() {
    // A crash would hand a core file, as commands shape it, to the host's
    // handler, such as systemd-coredump running as root; at one byte the
    // kernel runs none.
    use nix::sys::resource::{getrlimit, setrlimit, Resource, RLIM_INFINITY};
    let out = inside(&system(), || {
        let (soft, hard) = getrlimit(Resource::RLIMIT_CORE)?;
        let raised = setrlimit(Resource::RLIMIT_CORE, RLIM_INFINITY, RLIM_INFINITY);
        Ok(format!("{soft} {hard} {}", raised.is_ok()))
    });
    assert_eq!(out.unwrap(), "1 1 false");
}

/// The error of `call` in the container of `sandbox`, its system calls
/// restricted for `network` as the tool executor does, or 0 when it
/// succeeds.
fn call_error<F>(sandbox: &Sandbox, network: NetworkAccess, call: F) -> i32
where
    F: Fn() -> libc::c_long + Send + Sync + 'static,
{
    let res = inside(sandbox, move || {
        restrict_syscalls(network)?;
        match call() {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(String::new()),
        }
    });
    res.map_or_else(|err| err.raw_os_error().unwrap(), |_| 0)
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

/// Open a TCP session to `port` with a Fast Open send, without `connect`.
fn send_fast_open(port: u16) -> libc::c_long {
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: plain system calls on a buffer and an address that outlive
    // them.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return -1;
        }
        libc::sendto(
            fd,
            b"x".as_ptr().cast(),
            1,
            libc::MSG_FASTOPEN,
            (&addr as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) as libc::c_long
    }
}

/// Like [`send_fast_open`], with `connect` deferring to the first write.
fn connect_fast_open(port: u16) -> libc::c_long {
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        },
        sin_zero: [0; 8],
    };
    let on: libc::c_int = 1;
    // SAFETY: plain system calls on values that outlive them.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return -1;
        }
        if libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_FASTOPEN_CONNECT,
            (&on as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) != 0
        {
            return -1;
        }
        if libc::connect(
            fd,
            (&addr as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) != 0
        {
            return -1;
        }
        libc::write(fd, b"x".as_ptr().cast(), 1) as libc::c_long
    }
}

#[test]
fn fast_open_keeps_to_allowed_ports() {
    let (blocked, port) = server();
    blocked.set_nonblocking(true).unwrap();
    let sandbox = system().allow_connect(free_port());

    // Landlock only checks `connect`: the send must fail as when the host
    // does not use Fast Open, so that programs connect instead.
    let err = call_error(&sandbox, NetworkAccess::Ports, move || send_fast_open(port));
    assert_eq!(err, libc::EOPNOTSUPP);
    // A deferred connect still goes through `connect`.
    let err = call_error(&sandbox, NetworkAccess::Ports, move || {
        connect_fast_open(port)
    });
    assert_eq!(err, libc::EACCES);
    assert!(blocked.accept().is_err(), "reached a blocked port");
}
