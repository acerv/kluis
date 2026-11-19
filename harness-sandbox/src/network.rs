// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Taking the network away from commands.

use std::ffi::CStr;
use std::io;

use nix::sched::{unshare, CloneFlags};
use nix::unistd::{getegid, geteuid};
use rustix::fs::{open, Mode, OFlags};

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
        let (uid, gid) = (geteuid(), getegid());
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
        unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNET).map_err(io::Error::from)?;
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
    let fd =
        open(path, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty()).map_err(io::Error::from)?;
    rustix::io::write(&fd, data).map_err(io::Error::from)?;
    Ok(())
}
