// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Taking the network away from commands.

use std::ffi::CStr;
use std::io;

use crate::protect::set_dumpable;

/// Takes the network away from a process, e.g. a command before `exec`.
///
/// The process moves to new user and network namespaces, keeping its user
/// and group ids. It is left with a loopback device that is down, so no
/// socket can reach the host or other machines, including over UDP.
#[derive(Debug, Clone)]
pub struct NoNetwork {
    uid_map: String,
    gid_map: String,
}

impl NoNetwork {
    /// Prepare for the current user and group.
    pub fn new() -> Self {
        // SAFETY: these calls cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            uid_map: format!("{uid} {uid} 1"),
            gid_map: format!("{gid} {gid} 1"),
        }
    }

    /// Apply to the calling process, which must have a single thread.
    ///
    /// Only makes system calls, so it is safe to use in
    /// `Command::pre_exec`.
    pub fn apply(&self) -> io::Result<()> {
        // Children of a protected harness are not dumpable until `exec`,
        // which keeps them from writing their own id maps. Their memory is
        // replaced right after, so it is safe to drop the protection.
        set_dumpable(true)?;
        // SAFETY: plain system call.
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
            return Err(io::Error::last_os_error());
        }
        write(c"/proc/self/setgroups", b"deny")?;
        write(c"/proc/self/uid_map", self.uid_map.as_bytes())?;
        write(c"/proc/self/gid_map", self.gid_map.as_bytes())
    }
}

impl Default for NoNetwork {
    fn default() -> Self {
        Self::new()
    }
}

/// Write `data` to the file at `path` with a single system call.
fn write(path: &CStr, data: &[u8]) -> io::Result<()> {
    // SAFETY: `path` is a C string and `data` a valid buffer; the
    // descriptor is closed before returning.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = libc::write(fd, data.as_ptr().cast(), data.len());
        let result = if written < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        libc::close(fd);
        result
    }
}
