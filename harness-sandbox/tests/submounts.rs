// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A FUSE or network mount of the host below a shown folder must not
//! arrive with it: commands would reach the network through it.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use harness_sandbox::{GitGuard, Sandbox};

/// Set in the test that re-runs itself in a user and mount namespace.
const CHILD: &str = "HARNESS_TEST_SUBMOUNTS_CHILD";

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

/// Run `script` with bash in the container of `sandbox`, returning its
/// standard output and error.
fn run(sandbox: &Sandbox, script: &str) -> (String, String) {
    let out = sandbox
        .container()
        .unwrap()
        .command("/bin/bash")
        .args(["-c", script])
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run `body` in a user and mount namespace, where we can mount a tmpfs like
/// the host does, and skip the test when namespaces are unavailable.
fn in_namespace(name: &str, body: fn(&Path)) {
    if std::env::var_os(CHILD).is_some() {
        let dir = tempfile::tempdir().unwrap();
        body(dir.path());
        return;
    }
    let can_mount = Command::new("unshare")
        .args(["-Urm", "true"])
        .status()
        .is_ok_and(|status| status.success());
    if !can_mount {
        eprintln!("skipping {name}: cannot create a user namespace");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("unshare")
        .args(["-Urm", exe.to_str().unwrap(), "--exact", name])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{name} failed inside the namespace");
}

/// A home folder holding `mount.txt` next to a `drive` folder, as a cloud
/// drive sits below a home folder.
fn home(dir: &Path) -> (PathBuf, PathBuf) {
    let drive = dir.join("drive");
    std::fs::create_dir(&drive).unwrap();
    std::fs::write(dir.join("mount.txt"), "plain").unwrap();
    (dir.to_path_buf(), drive)
}

/// A FUSE filesystem with an empty root folder, served by a thread that
/// records the names it is asked to look up: what a cloud drive would send
/// to its servers.
struct Fuse {
    lookups: Arc<Mutex<Vec<String>>>,
}

impl Fuse {
    /// Mount on `target`, serving it while `serve` holds, or with the
    /// connection closed at once, as when the program serving it died.
    fn mount(target: &Path, serve: bool) -> Self {
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
            .unwrap();
        let options = format!(
            "fd={},rootmode=40000,user_id=0,group_id=0",
            device.as_raw_fd()
        );
        let c = |s: &[u8]| CString::new(s).unwrap();
        let (source, fstype) = (c(b"drive"), c(b"fuse"));
        let (target_c, options) = (c(target.as_os_str().as_bytes()), c(options.as_bytes()));
        // SAFETY: plain system call with valid C strings.
        let res = unsafe {
            libc::mount(
                source.as_ptr(),
                target_c.as_ptr(),
                fstype.as_ptr(),
                0,
                options.as_ptr().cast(),
            )
        };
        assert_eq!(res, 0, "mount fuse: {}", std::io::Error::last_os_error());

        let lookups = Arc::new(Mutex::new(Vec::new()));
        if serve {
            let log = lookups.clone();
            std::thread::spawn(move || serve_fuse(device, &log));
        }
        Self { lookups }
    }

    fn lookups(&self) -> Vec<String> {
        self.lookups.lock().unwrap().clone()
    }
}

fn mount_tmpfs(target: &Path) {
    let status = Command::new("mount")
        .args(["-t", "tmpfs", "none"])
        .arg(target)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "cannot mount tmpfs on {}",
        target.display()
    );
}

const FUSE_LOOKUP: u32 = 1;
const FUSE_FORGET: u32 = 2;
const FUSE_GETATTR: u32 = 3;
const FUSE_INIT: u32 = 26;
const FUSE_INTERRUPT: u32 = 36;
const FUSE_BATCH_FORGET: u32 = 42;

/// Answer FUSE requests on `device` until it is unmounted: set up the
/// connection, describe the root folder, record looked up names and
/// refuse everything else.
fn serve_fuse(mut device: std::fs::File, lookups: &Mutex<Vec<String>>) {
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let Ok(n) = device.read(&mut buf) else {
            return;
        };
        let request = &buf[..n];
        let u32_at = |at: usize| u32::from_ne_bytes(request[at..at + 4].try_into().unwrap());
        let opcode = u32_at(4);
        let unique = &request[8..16];
        let body = &request[40..];
        let reply = match opcode {
            FUSE_FORGET | FUSE_BATCH_FORGET | FUSE_INTERRUPT => continue,
            FUSE_INIT => {
                // fuse_init_out: version 7.31, 4 KiB writes, nothing else.
                let mut out = vec![0u8; 64];
                out[0..4].copy_from_slice(&7u32.to_ne_bytes());
                out[4..8].copy_from_slice(&31u32.to_ne_bytes());
                out[20..24].copy_from_slice(&4096u32.to_ne_bytes());
                Ok(out)
            }
            FUSE_GETATTR => {
                // fuse_attr_out: an empty folder, as inode 1.
                let mut out = vec![0u8; 104];
                out[16..24].copy_from_slice(&1u64.to_ne_bytes());
                out[76..80].copy_from_slice(&0o40755u32.to_ne_bytes());
                out[80..84].copy_from_slice(&2u32.to_ne_bytes());
                Ok(out)
            }
            FUSE_LOOKUP => {
                let name = body.split(|&b| b == 0).next().unwrap_or_default();
                lookups
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(name).into_owned());
                Err(libc::ENOENT)
            }
            _ => Err(libc::ENOSYS),
        };
        let (error, payload) = match reply {
            Ok(payload) => (0, payload),
            Err(errno) => (-errno, Vec::new()),
        };
        let mut out = Vec::with_capacity(16 + payload.len());
        out.extend_from_slice(&(16 + payload.len() as u32).to_ne_bytes());
        out.extend_from_slice(&error.to_ne_bytes());
        out.extend_from_slice(unique);
        out.extend_from_slice(&payload);
        if device.write_all(&out).is_err() {
            return;
        }
    }
}

#[test]
fn fuse_mount_is_hidden() {
    in_namespace("fuse_mount_is_hidden", |dir| {
        let (dir, drive) = home(dir);
        let fuse = Fuse::mount(&drive, true);
        let sandbox = system().allow_read(&dir);

        // The folder itself stays readable; the mount below it does not.
        let (out, err) = run(&sandbox, &format!("cat {}/mount.txt", dir.display()));
        assert_eq!(out, "plain", "{err}");

        // Nothing reaches the program serving it. (As root of the test's
        // user namespace, the command sees through the cover's permissions:
        // to it, the folder is just empty.)
        let (_, err) = run(&sandbox, &format!("ls {}/leak-me", drive.display()));
        assert!(!err.is_empty());
        assert!(!fuse.lookups().contains(&"leak-me".into()));
    });
}

#[test]
fn fuse_mount_in_writable_folder_is_hidden() {
    in_namespace("fuse_mount_in_writable_folder_is_hidden", |dir| {
        let (dir, drive) = home(dir);
        let fuse = Fuse::mount(&drive, true);
        let sandbox = system().allow_write(&dir);

        let (_, err) = run(&sandbox, &format!("touch {}/leak-me", drive.display()));
        assert!(!err.is_empty());
        assert!(!fuse.lookups().contains(&"leak-me".into()));
    });
}

#[test]
fn hidden_fuse_mounts_are_not_checked_for_git() {
    // Commands cannot reach them, and a live one would get the requests.
    in_namespace("hidden_fuse_mounts_are_not_checked_for_git", |dir| {
        let (dir, drive) = home(dir);
        let dead = dir.join("dead");
        std::fs::create_dir(&dead).unwrap();
        let fuse = Fuse::mount(&drive, true);
        Fuse::mount(&dead, false);
        let sandbox = system().allow_write(&dir);

        let mut guard = GitGuard::new(&sandbox).unwrap();
        let check = guard.disable_new();
        assert!(check.new.is_empty() && check.unchecked.is_none());
        let (out, err) = run(&sandbox, &format!("cat {}/mount.txt", dir.display()));
        assert_eq!(out, "plain", "{err}");
        assert!(fuse.lookups().is_empty(), "{:?}", fuse.lookups());
    });
}

#[test]
fn allowed_fuse_mount_is_shown() {
    in_namespace("allowed_fuse_mount_is_shown", |dir| {
        let (dir, drive) = home(dir);
        let fuse = Fuse::mount(&drive, true);
        let sandbox = system().allow_read(&dir).allow_read(&drive);

        run(&sandbox, &format!("ls {}/shown", drive.display()));
        assert!(fuse.lookups().contains(&"shown".into()));
    });
}

#[test]
fn dead_fuse_mount_does_not_stop_the_sandbox() {
    // Its program is gone: nothing behind it to reach.
    in_namespace("dead_fuse_mount_does_not_stop_the_sandbox", |dir| {
        let (dir, drive) = home(dir);
        Fuse::mount(&drive, false);
        let sandbox = system().allow_read(&dir);

        let (out, err) = run(&sandbox, &format!("cat {}/mount.txt", dir.display()));
        assert_eq!(out, "plain", "{err}");
    });
}

#[test]
fn local_mount_over_fuse_is_shown() {
    // Only the last mount on a path shows, as a disk over its automount
    // point.
    in_namespace("local_mount_over_fuse_is_shown", |dir| {
        let (dir, drive) = home(dir);
        let _fuse = Fuse::mount(&drive, true);
        mount_tmpfs(&drive);
        std::fs::write(drive.join("file.txt"), "local").unwrap();
        let sandbox = system().allow_read(&dir);

        let (out, err) = run(&sandbox, &format!("cat {}/file.txt", drive.display()));
        assert_eq!(out, "local", "{err}");
    });
}

#[test]
fn fuse_mount_with_a_space_is_hidden() {
    // Paths in the mount table escape spaces.
    in_namespace("fuse_mount_with_a_space_is_hidden", |dir| {
        let drive = dir.join("my drive");
        std::fs::create_dir(&drive).unwrap();
        let fuse = Fuse::mount(&drive, true);
        let sandbox = system().allow_read(dir);

        run(&sandbox, &format!("ls '{}/leak-me'", drive.display()));
        assert!(!fuse.lookups().contains(&"leak-me".into()));
    });
}

#[test]
fn local_mount_is_shown() {
    // As /usr/local on its own disk or subvolume below /usr.
    in_namespace("local_mount_is_shown", |dir| {
        let (dir, drive) = home(dir);
        mount_tmpfs(&drive);
        std::fs::write(drive.join("file.txt"), "local").unwrap();
        let sandbox = system().allow_read(&dir);

        let (out, err) = run(&sandbox, &format!("cat {}/file.txt", drive.display()));
        assert_eq!(out, "local", "{err}");
    });
}
