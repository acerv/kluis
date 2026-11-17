// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Filesystem and network sandbox for LLM harnesses, based on Linux
//! Landlock (Linux 6.7 or newer).
//!
//! A [`Sandbox`] lists the folders the harness may read and write and the
//! TCP ports it may connect to or bind, or allows any network; everything
//! else is denied by the kernel once the sandbox is applied. UDP and Unix
//! sockets are not filtered.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-sandbox only supports Linux");

mod profile;

pub use profile::{Options, Profile};

use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use hakoniwa::landlock::{CompatMode, Resource};
use hakoniwa::{Container, Runctl};
use landlock::{
    Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
    Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
};

/// Landlock ABI the rules need: filesystem and TCP rules (Linux 6.7).
const TARGET_ABI: ABI = ABI::V4;
const TARGET_ABI_VERSION: i64 = 4;

/// Always readable and writable: nearly every program needs it, and
/// spawning a process with closed stdin opens it.
const DEV_NULL: &str = "/dev/null";

/// Folders and network the harness may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    network: Network,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Network {
    /// Only these TCP ports; no network when both are empty.
    Ports { connect: Vec<u16>, bind: Vec<u16> },
    /// No TCP filtering.
    Any,
}

impl Default for Network {
    fn default() -> Self {
        Network::Ports {
            connect: Vec::new(),
            bind: Vec::new(),
        }
    }
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
        if let Network::Ports { connect, .. } = &mut self.network {
            connect.push(port);
        }
        self
    }

    /// Allow binding TCP sockets to `port`.
    pub fn allow_bind(mut self, port: u16) -> Self {
        if let Network::Ports { bind, .. } = &mut self.network {
            bind.push(port);
        }
        self
    }

    /// Allow any TCP connection and bind; port rules no longer matter.
    pub fn allow_any_network(mut self) -> Self {
        self.network = Network::Any;
        self
    }

    /// A container showing only the allowed paths, at the same place, plus
    /// `/dev`, `/proc` and a private `/tmp`. Commands run in it as the
    /// current user, on the host network.
    ///
    /// Fails if a path is missing or not UTF-8. Starting a command in it
    /// fails without user namespaces, or without Landlock TCP support
    /// (Linux 6.7) unless any network is allowed.
    pub fn container(&self) -> Result<Container, SandboxError> {
        let mut container = Container::new();
        container
            .runctl(Runctl::MountFallback)
            .devfsmount("/dev")
            .tmpfsmount("/tmp");
        for (path, writable) in self.mounts()? {
            if writable {
                container.bindmount_rw(&path, &path);
            } else {
                container.bindmount_ro(&path, &path);
            }
        }
        if let Network::Ports { connect, bind } = &self.network {
            let mut ruleset = hakoniwa::landlock::Ruleset::default();
            ruleset
                .restrict(Resource::NET_TCP_CONNECT, CompatMode::Enforce)
                .restrict(Resource::NET_TCP_BIND, CompatMode::Enforce);
            for &port in connect {
                ruleset.allow_tcp_connect(port);
            }
            for &port in bind {
                ruleset.allow_tcp_bind(port);
            }
            container.landlock_ruleset(ruleset);
        }
        Ok(container)
    }

    /// Absolute paths to mount and whether they are writable, without the
    /// ones already shown by a mount of a parent folder.
    fn mounts(&self) -> Result<Vec<(String, bool)>, SandboxError> {
        let mut all = Vec::new();
        for (paths, writable) in [(&self.read, false), (&self.write, true)] {
            for path in paths {
                let error = |e: &dyn fmt::Display| SandboxError {
                    message: format!("{}: {e}", path.display()),
                };
                let path = std::path::absolute(path).map_err(|e| error(&e))?;
                std::fs::metadata(&path).map_err(|e| error(&e))?;
                if path.to_str().is_none() {
                    return Err(error(&"not UTF-8"));
                }
                all.push((path, writable));
            }
        }
        // A mount inside another one would create its mount point on the
        // host.
        let covered = |(path, writable): &(PathBuf, bool)| {
            all.iter()
                .any(|(other, w)| other != path && path.starts_with(other) && (*w || !writable))
        };
        Ok(all
            .iter()
            .filter(|m| !covered(m))
            .filter_map(|(path, writable)| Some((path.to_str()?.to_owned(), *writable)))
            .collect())
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
        if abi_version() < TARGET_ABI_VERSION {
            return Err(landlock_error(
                "Linux 6.7 or newer with Landlock enabled is required",
            ));
        }
        let fs = AccessFs::from_all(TARGET_ABI);

        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(fs)
            .map_err(landlock_error)?;
        if let Network::Ports { .. } = self.network {
            ruleset = ruleset
                .handle_access(AccessNet::from_all(TARGET_ABI))
                .map_err(landlock_error)?;
        }
        let mut ruleset = ruleset
            .create()
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
        if let Network::Ports { connect, bind } = &self.network {
            for (ports, access) in [(connect, AccessNet::ConnectTcp), (bind, AccessNet::BindTcp)] {
                for &port in ports {
                    ruleset = ruleset
                        .add_rule(NetPort::new(port, access))
                        .map_err(landlock_error)?;
                }
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

/// Landlock ABI version of the running kernel; 0 or less without Landlock.
fn abi_version() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: with this flag the call only returns the version.
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
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
