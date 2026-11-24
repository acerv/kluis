// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Filesystem and network sandbox for LLM harnesses, based on hakoniwa
//! containers.
//!
//! A [`Sandbox`] lists the folders the harness may read and write and the
//! TCP ports it may connect to, or allows any network. Its
//! [`container`](Sandbox::container) shows only those folders, and
//! Landlock (Linux 6.7 or newer) limits TCP to those ports, while
//! [`restrict_syscalls`] refuses UDP; [`Sandbox::no_network`] takes away
//! all network.
//!
//! Programs of the host that can reach the network are kept away too: a
//! mount below a shown folder whose filesystem could reach the network,
//! such as a cloud drive or a network share, is hidden unless it is
//! allowed itself, and [`restrict_syscalls`], called inside the
//! container, keeps commands off the Unix sockets of the host and out of
//! new user namespaces.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-sandbox only supports Linux");

pub mod apparmor;
mod env;
mod git;
mod init;
mod mountinfo;
mod mounts;
mod profile;
mod protect;
mod seccomp;

pub use env::command_env;
pub use git::{GitCheck, GitGuard, NewRepository};
pub use init::{start_init, stop_others};
pub use mountinfo::remote_filesystem;
pub use profile::{hidden_secrets, Options};
pub use protect::{kill_first_on_oom, protect_self, refuse_root};
pub use seccomp::{restrict_syscalls, NetworkAccess};

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use hakoniwa::landlock::{CompatMode, Resource, Ruleset};
use hakoniwa::{Container, MountOptions, Namespace, Rlimit, Runctl};
use thiserror::Error;

use mounts::MountKind;

/// Folders and network the harness may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    read_only: Vec<PathBuf>,
    deny: Vec<PathBuf>,
    private: Vec<PathBuf>,
    network: Network,
    max_processes: Option<u64>,
    max_tmp_size: Option<u64>,
}

/// Core file size limit of commands, in bytes: see [`Sandbox::container`].
const CORE_LIMIT: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Network {
    /// Private network namespace (`CLONE_NEWNET`) with no interfaces up.
    None,
    /// Only connections to these TCP ports, and no bind.
    Ports(Vec<u16>),
    /// No TCP filtering.
    Any,
}

impl Default for Network {
    fn default() -> Self {
        Network::Ports(Vec::new())
    }
}

/// Error raised while building a sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct SandboxError {
    pub message: String,
}

impl SandboxError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The `problem` with `path`.
    pub(crate) fn at(path: &Path, problem: impl fmt::Display) -> Self {
        Self::new(format!("{}: {problem}", path.display()))
    }
}

impl Sandbox {
    /// A sandbox that allows nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow reading and executing anything below `path`.
    ///
    /// A FUSE or network mount below `path` stays hidden unless it is
    /// allowed too.
    pub fn allow_read(mut self, path: impl Into<PathBuf>) -> Self {
        self.read.push(path.into());
        self
    }

    /// Allow reading, executing and changing anything below `path`, but
    /// any `.git` below it, which stays read-only unless allowed itself:
    /// git on the host would otherwise run the hooks and commands written
    /// there.
    ///
    /// A FUSE or network mount below `path` stays hidden unless it is
    /// allowed too.
    pub fn allow_write(mut self, path: impl Into<PathBuf>) -> Self {
        self.write.push(path.into());
        self
    }

    /// Allow reading and executing anything below `path`, and keep it
    /// read-only even below, at, or above a writable path. The folders
    /// between a writable parent and `path` can then no longer be moved or
    /// removed, which would let a new file take its place.
    pub fn keep_read_only(mut self, path: impl Into<PathBuf>) -> Self {
        self.read_only.push(path.into());
        self
    }

    /// Hide `path` even below, at, or above a readable or writable path.
    /// A missing path is ignored.
    pub fn deny_read(mut self, path: impl Into<PathBuf>) -> Self {
        self.deny.push(path.into());
        self
    }

    /// Show an empty, writable folder of this user at `path`, private to
    /// the container and gone with it. Allowed paths below `path` still show
    /// on top of it. It lives in memory and holds at most what
    /// [`Sandbox::max_tmp_size`] lets the private `/tmp` hold.
    pub fn private_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.private.push(path.into());
        self
    }

    /// Isolate the container in an empty network namespace (`CLONE_NEWNET`)
    /// so no TCP, UDP, or host abstract Unix sockets are reachable.
    pub fn no_network(mut self) -> Self {
        self.network = Network::None;
        self
    }

    /// Allow TCP connections to `port` on any host.
    pub fn allow_connect(mut self, port: u16) -> Self {
        match &mut self.network {
            Network::Ports(ports) => ports.push(port),
            Network::None => self.network = Network::Ports(vec![port]),
            Network::Any => {}
        }
        self
    }

    /// Allow any TCP connection and bind; port rules no longer matter.
    pub fn allow_any_network(mut self) -> Self {
        self.network = Network::Any;
        self
    }

    /// Let the processes and threads of the container number at most `max`,
    /// so a fork bomb cannot fill the host.
    ///
    /// Uses `RLIMIT_NPROC`, which Linux 5.14 and newer count per user
    /// namespace: processes of the same user outside the container do not
    /// count. Older kernels count all of them.
    pub fn max_processes(mut self, max: u64) -> Self {
        self.max_processes = Some(max);
        self
    }

    /// Let the private `/tmp`, and each [`Sandbox::private_dir`], hold at
    /// most `bytes`; they live in memory.
    pub fn max_tmp_size(mut self, bytes: u64) -> Self {
        self.max_tmp_size = Some(bytes);
        self
    }

    /// A container showing only the allowed paths, at the same place, plus
    /// `/dev`, `/proc` and a private `/tmp`, with its own IPC and host name.
    /// Commands run in it as the current user, on the host network, and
    /// leave no core dumps.
    ///
    /// Fails if a path is missing or not UTF-8. Starting a command in it
    /// fails without user namespaces, or without Landlock TCP support
    /// (Linux 6.7) unless any network is allowed.
    pub fn container(&self) -> Result<Container, SandboxError> {
        if self.max_tmp_size == Some(0) {
            return Err(SandboxError::new("max_tmp_size: must be at least 1"));
        }
        let mut container = Container::new();
        container
            // Their own SysV shared memory, semaphores and message queues,
            // which would otherwise be the host's and outlive the container,
            // and their own host name.
            .unshare(Namespace::Ipc)
            .unshare(Namespace::Uts)
            .runctl(Runctl::MountFallback)
            .devfsmount("/dev")
            .mount(
                "tmpfs",
                "/tmp",
                "tmpfs",
                MountOptions::NOSUID | MountOptions::NODEV,
                self.max_tmp_size.map(|bytes| format!("size={bytes}")),
            );
        let size = self
            .max_tmp_size
            .map_or(String::new(), |bytes| format!(",size={bytes}"));
        for path in &self.private {
            let path = absolute_utf8(path)?;
            container.mount(
                "tmpfs",
                &path,
                "tmpfs",
                MountOptions::NOSUID | MountOptions::NODEV,
                Some(format!("mode=700{size}")),
            );
        }
        for (path, kind) in self.mounts()? {
            match kind {
                MountKind::Bind { writable: true } => {
                    container.bindmount_rw(&path, &path);
                }
                MountKind::Bind { writable: false } => {
                    container.bindmount_ro(&path, &path);
                }
                MountKind::MaskDir => {
                    container.mount(
                        "tmpfs",
                        &path,
                        "tmpfs",
                        MountOptions::RDONLY | MountOptions::NOSUID | MountOptions::NODEV,
                        Some("size=0,mode=000".to_string()),
                    );
                }
                MountKind::MaskFile => {
                    container.mount(
                        "/dev/null",
                        &path,
                        "",
                        MountOptions::BIND
                            | MountOptions::RDONLY
                            | MountOptions::NOSUID
                            | MountOptions::NODEV,
                        None,
                    );
                }
            }
        }
        // A crash would hand a core file, as commands shape it, to the
        // host's handler, such as systemd-coredump running as root. One byte
        // is the limit at which the kernel runs no core handler at all; as
        // the hard limit too, commands cannot raise it.
        container.setrlimit(Rlimit::Core, CORE_LIMIT, CORE_LIMIT);
        if let Some(max) = self.max_processes {
            if max == 0 {
                return Err(SandboxError::new("max_processes: must be at least 1"));
            }
            container.setrlimit(Rlimit::Nproc, max, max);
        }
        match &self.network {
            Network::None => {
                container.unshare(Namespace::Network);
            }
            Network::Ports(ports) => {
                let mut ruleset = Ruleset::default();
                ruleset
                    .restrict(Resource::NET_TCP_CONNECT, CompatMode::Enforce)
                    .restrict(Resource::NET_TCP_BIND, CompatMode::Enforce);
                for &port in ports {
                    ruleset.allow_tcp_connect(port);
                }
                container.landlock_ruleset(ruleset);
            }
            Network::Any => {}
        }
        Ok(container)
    }
}

/// Limit the virtual memory of the calling process and its children to
/// `bytes`, e.g. a command before `exec`.
///
/// Only makes a system call, so it is safe to use in `Command::pre_exec`.
pub fn limit_memory(bytes: u64) -> io::Result<()> {
    nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_AS, bytes, bytes)
        .map_err(io::Error::from)
}

/// `path` made absolute, as UTF-8.
fn absolute_utf8(path: &Path) -> Result<String, SandboxError> {
    let error = |e: &dyn fmt::Display| SandboxError::at(path, e);
    std::path::absolute(path)
        .map_err(|e| error(&e))?
        .into_os_string()
        .into_string()
        .map_err(|_| error(&"not UTF-8"))
}
