// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Filesystem and network sandbox for LLM harnesses, based on Linux
//! Landlock (Linux 6.7 or newer).
//!
//! A [`Sandbox`] lists the folders the harness may read and write and the
//! TCP ports it may connect to or bind; everything else is denied by the
//! kernel once the sandbox is applied. UDP and Unix sockets are not
//! filtered.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-sandbox only supports Linux");

use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use landlock::{
    Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
    Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
};

/// Landlock ABI the rules need: filesystem and TCP rules (Linux 6.7).
const TARGET_ABI: ABI = ABI::V4;

/// Always readable and writable: nearly every program needs it, and
/// spawning a process with closed stdin opens it.
const DEV_NULL: &str = "/dev/null";

/// Folders and TCP ports the harness may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    connect: Vec<u16>,
    bind: Vec<u16>,
}

/// A sandbox ready to be applied, e.g. in a child process before `exec`.
#[derive(Debug)]
pub struct Prepared {
    ruleset: OwnedFd,
}

/// Error raised while building or applying a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxError {
    pub message: String,
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SandboxError {}

impl Sandbox {
    /// A sandbox that allows nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow reading and executing anything below `path`.
    pub fn allow_read(mut self, path: impl Into<PathBuf>) -> Self {
        self.read.push(path.into());
        self
    }

    /// Allow reading, executing and changing anything below `path`.
    pub fn allow_write(mut self, path: impl Into<PathBuf>) -> Self {
        self.write.push(path.into());
        self
    }

    /// Allow TCP connections to `port` on any host.
    pub fn allow_connect(mut self, port: u16) -> Self {
        self.connect.push(port);
        self
    }

    /// Allow binding TCP sockets to `port`.
    pub fn allow_bind(mut self, port: u16) -> Self {
        self.bind.push(port);
        self
    }

    /// Restrict the calling thread, and every thread and process it starts
    /// afterwards, to this sandbox and `/dev/null`. This cannot be undone.
    ///
    /// Threads that already exist are not restricted: call it before
    /// starting any, e.g. before building the async runtime.
    pub fn apply(&self) -> Result<(), SandboxError> {
        self.prepare()?.restrict_self().map_err(landlock_error)
    }

    /// Build the sandbox without applying it, so it can be applied later
    /// where allocating is not allowed, such as between `fork` and `exec`.
    ///
    /// Fails if the kernel lacks Landlock filesystem and TCP support.
    pub fn prepare(&self) -> Result<Prepared, SandboxError> {
        let fs = AccessFs::from_all(TARGET_ABI);
        let net = AccessNet::from_all(TARGET_ABI);

        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(fs)
            .and_then(|r| r.handle_access(net))
            .and_then(Ruleset::create)
            .map_err(landlock_error)?
            .add_rule(rule(Path::new(DEV_NULL), fs)?)
            .map_err(landlock_error)?;
        for (paths, access) in [
            (&self.read, AccessFs::from_read(TARGET_ABI)),
            (&self.write, fs),
        ] {
            for path in paths {
                ruleset = ruleset
                    .add_rule(rule(path, access)?)
                    .map_err(landlock_error)?;
            }
        }
        for (ports, access) in [
            (&self.connect, AccessNet::ConnectTcp),
            (&self.bind, AccessNet::BindTcp),
        ] {
            for &port in ports {
                ruleset = ruleset
                    .add_rule(NetPort::new(port, access))
                    .map_err(landlock_error)?;
            }
        }

        let ruleset: Option<OwnedFd> = ruleset.into();
        let ruleset = ruleset.ok_or_else(|| landlock_error("not supported by the kernel"))?;
        Ok(Prepared { ruleset })
    }
}

impl Prepared {
    /// Restrict the calling thread, and every thread and process it starts
    /// afterwards. This cannot be undone.
    ///
    /// Only makes two system calls, so it is safe to use in
    /// `Command::pre_exec`. The ruleset is closed on `exec`.
    pub fn restrict_self(&self) -> io::Result<()> {
        // SAFETY: plain system calls on a ruleset descriptor we own.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// A rule granting `access` below `path`, which must exist.
fn rule(path: &Path, access: BitFlags<AccessFs>) -> Result<PathBeneath<PathFd>, SandboxError> {
    let fd = PathFd::new(path).map_err(|e| SandboxError {
        message: format!("{}: {e}", path.display()),
    })?;
    // Rules on a file accept only the rights that apply to files.
    let access = if path.is_dir() {
        access
    } else {
        access & AccessFs::from_file(TARGET_ABI)
    };
    Ok(PathBeneath::new(fd, access))
}

fn landlock_error(e: impl fmt::Display) -> SandboxError {
    SandboxError {
        message: format!("landlock: {e}"),
    }
}
