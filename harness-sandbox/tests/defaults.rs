// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::{connect, errno, fs_size, inside, list, read, write};
use harness_sandbox::{Options, Sandbox};
use libc::{EACCES, ENETUNREACH, ENOENT, EROFS};
use nix::sys::resource::{getrlimit, Resource};

/// The default sandbox for `options`.
fn sandbox(options: &Options) -> Sandbox {
    Sandbox::with_defaults(options)
}

fn server() -> (TcpListener, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[test]
fn programs_run_in_given_folders() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        write: vec![dir.path().into()],
        ..Options::default()
    };

    let file = dir.path().join("f.txt");
    let out = sandbox(&options)
        .container()
        .unwrap()
        .command("/usr/bin/touch")
        .arg(file.to_str().unwrap())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(file.exists());
}

#[test]
fn dns_files_are_readable() {
    for path in ["/etc/hosts", "/etc/os-release", "/etc/services"] {
        if Path::new(path).exists() {
            let res = inside(&sandbox(&Options::default()), read(Path::new(path)));
            assert!(res.is_ok(), "{path}: {res:?}");
        }
    }
    if Path::new("/etc/alternatives").exists() {
        let dir = Path::new("/etc/alternatives");
        inside(&sandbox(&Options::default()), list(dir)).unwrap();
    }
}

#[test]
fn cannot_write_elsewhere() {
    for path in ["/f", "/usr/f"] {
        let res = inside(&sandbox(&Options::default()), write(Path::new(path), "x"));
        assert_eq!(errno(res), EROFS, "{path}");
    }
}

#[test]
fn default_isolates_all_network() {
    let (_server, port) = server();
    let res = inside(&sandbox(&Options::default()), connect(port));
    assert_eq!(errno(res), ENETUNREACH);
}

#[test]
fn connect_to_listed_ports_only() {
    let (_server, port) = server();
    let (_other, other) = server();
    let options = Options {
        connect: vec![port],
        ..Options::default()
    };

    let sandbox = sandbox(&options);
    inside(&sandbox, connect(port)).unwrap();
    assert_eq!(errno(inside(&sandbox, connect(other))), EACCES);
}

#[test]
fn network_option_allows_any_port() {
    let (_server, port) = server();
    let options = Options {
        network: true,
        ..Options::default()
    };

    inside(&sandbox(&options), connect(port)).unwrap();
}

/// A folder in the real `$HOME` holding `file.txt`.
fn home_folder() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    std::fs::write(dir.path().join("file.txt"), "data").unwrap();
    dir
}

#[test]
fn home_shows_allowed_paths_only() {
    let shown = home_folder();
    let hidden = home_folder();
    let options = Options {
        read: vec![shown.path().into()],
        ..Options::default()
    };

    let sandbox = sandbox(&options);
    let file = shown.path().join("file.txt");
    assert_eq!(inside(&sandbox, read(&file)).unwrap(), "data");
    assert_eq!(errno(inside(&sandbox, list(hidden.path()))), ENOENT);
}

#[test]
fn home_is_private_and_writable() {
    let hidden = home_folder();
    let name = hidden.path().file_name().unwrap().to_str().unwrap();
    let home = Path::new(&std::env::var("HOME").unwrap()).to_path_buf();
    let path = home.clone();
    let out = inside(&sandbox(&Options::default()), move || {
        let names = list(&path)()?;
        let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        std::fs::write(path.join(".harness-test"), "x")?;
        Ok(format!("{names}{mode:o}"))
    })
    .unwrap();

    assert!(!out.contains(name), "{out}");
    assert!(out.ends_with("700"), "{out}");
    assert!(!home.join(".harness-test").exists());
}

#[test]
fn whole_home_can_be_shown_without_secrets() {
    let dir = home_folder();
    let secret = Path::new(&std::env::var("HOME").unwrap()).join(".ssh");
    let options = Options {
        read: vec![std::env::var("HOME").unwrap().into()],
        ..Options::default()
    };

    let sandbox = sandbox(&options);
    let file = dir.path().join("file.txt");
    assert_eq!(inside(&sandbox, read(&file)).unwrap(), "data");
    let res = inside(&sandbox, write(&dir.path().join("x"), "x"));
    assert_eq!(errno(res), EROFS);
    if secret.exists() {
        assert_eq!(inside(&sandbox, list(&secret)).unwrap(), "");
    }
}

/// How many processes the user can have in the sandbox for `options`.
fn max_processes(options: &Options) -> u64 {
    let out = inside(&sandbox(options), || {
        Ok(getrlimit(Resource::RLIMIT_NPROC)?.0.to_string())
    });
    out.unwrap().parse().unwrap()
}

#[test]
fn processes_are_limited() {
    assert_eq!(max_processes(&Options::default()), 1024);

    let options = Options {
        max_processes: Some(256),
        ..Options::default()
    };
    assert_eq!(max_processes(&options), 256);
}

#[test]
fn tmp_size_is_configurable() {
    let options = Options {
        tmp_size: Some(1 << 20),
        ..Options::default()
    };
    assert_eq!(tmp_size(&options), 1 << 20);
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
    let sandbox = sandbox(&options);
    assert_eq!(inside(&sandbox, read(&public)).unwrap(), "public\n");
    assert_eq!(errno(inside(&sandbox, list(&secret))), EACCES);
}

/// Size of the private `/tmp` for `options`, in bytes.
fn tmp_size(options: &Options) -> u64 {
    let size = inside(&sandbox(options), fs_size(Path::new("/tmp")));
    size.unwrap().parse().unwrap()
}

#[test]
fn tmp_holds_a_quarter_of_the_memory() {
    // It lives in memory, which killing its writers does not free.
    let quarter = nix::sys::sysinfo::sysinfo().unwrap().ram_total() / 4;
    let size = tmp_size(&Options::default());
    assert!(quarter.abs_diff(size) < 1 << 20, "{size} != {quarter}");
}

#[test]
fn tmp_size_can_be_chosen() {
    let options = Options {
        tmp_size: Some(8 << 20),
        ..Options::default()
    };
    assert_eq!(tmp_size(&options), 8 << 20);
}
