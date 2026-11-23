// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Run Rust in a container, where it meets the sandbox as programs do.

#![allow(dead_code)]

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Write};
use std::mem::ManuallyDrop;
use std::net::{Ipv4Addr, TcpStream};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use caps::CapSet;
use harness_sandbox::Sandbox;

/// System folders, as kl shows them.
pub fn system() -> Sandbox {
    let mut sandbox = Sandbox::new();
    for path in ["/usr", "/bin", "/lib", "/lib64"] {
        if Path::new(path).exists() {
            sandbox = sandbox.allow_read(path);
        }
    }
    sandbox
}

/// How hakoniwa exits when the container, or a panic in it, fails.
const FAILED: i32 = 125;

/// Run `body` in the container of `sandbox` as a program started there
/// would: the text it returns, or the error it fails with.
pub fn inside<F>(sandbox: &Sandbox, body: F) -> io::Result<String>
where
    F: Fn() -> io::Result<String> + Send + Sync + 'static,
{
    let container = sandbox.container().unwrap();
    // SAFETY: the closure runs in the forked child, which hakoniwa already
    // sets up with allocations of its own; it only makes plain system calls
    // and writes to its standard output.
    let mut command = unsafe {
        container.command_from_closure(move || match drop_capabilities().and_then(|()| body()) {
            Ok(text) => {
                // Not `print!`, which the test harness captures in memory.
                let mut stdout = ManuallyDrop::new(File::from_raw_fd(1));
                match stdout.write_all(text.as_bytes()) {
                    Ok(()) => 0,
                    Err(_) => FAILED,
                }
            }
            Err(err) => err.raw_os_error().unwrap_or(FAILED),
        })
    };
    let out = command.output().unwrap();
    match out.status.code {
        0 => Ok(String::from_utf8(out.stdout).unwrap()),
        FAILED => panic!(
            "{}: {}",
            out.status.reason,
            String::from_utf8_lossy(&out.stderr)
        ),
        errno => Err(io::Error::from_raw_os_error(errno)),
    }
}

/// Drop the capabilities that the container's user namespace gives, as
/// `execve` does for a program not run as root.
fn drop_capabilities() -> io::Result<()> {
    // SAFETY: a plain system call.
    if unsafe { libc::geteuid() } == 0 {
        return Ok(());
    }
    for set in [CapSet::Effective, CapSet::Permitted] {
        caps::clear(None, set).map_err(io::Error::other)?;
    }
    Ok(())
}

/// The errno `res` failed with.
pub fn errno(res: io::Result<String>) -> i32 {
    match res {
        Ok(text) => panic!("succeeded with {text:?}"),
        Err(err) => err.raw_os_error().unwrap(),
    }
}

/// Read the file at `path`.
pub fn read(path: &Path) -> impl Fn() -> io::Result<String> {
    let path = path.to_path_buf();
    move || std::fs::read_to_string(&path)
}

/// Write `text` to the file at `path`.
pub fn write(path: &Path, text: &str) -> impl Fn() -> io::Result<String> {
    let (path, text) = (path.to_path_buf(), text.to_string());
    move || std::fs::write(&path, &text).map(|()| String::new())
}

/// The names in the folder at `path`, sorted, one per line.
pub fn list(path: &Path) -> impl Fn() -> io::Result<String> {
    let path = path.to_path_buf();
    move || {
        let mut names = std::fs::read_dir(&path)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names.iter().map(|name| format!("{name}\n")).collect())
    }
}

/// The size of the file system holding `path`, in bytes.
pub fn fs_size(path: &Path) -> impl Fn() -> io::Result<String> {
    let path = path.to_path_buf();
    move || {
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: a plain system call on a string and a buffer that
        // outlive it.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((stat.f_blocks * stat.f_frsize).to_string())
    }
}

/// Connect to the local TCP `port`.
pub fn connect(port: u16) -> impl Fn() -> io::Result<String> {
    move || TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map(|_| String::new())
}
