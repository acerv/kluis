// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Filesystem and network sandbox for LLM harnesses, based on hakoniwa
//! containers.
//!
//! A [`Sandbox`] lists the folders the harness may read and write and the
//! TCP ports it may connect to, or allows any network. Its
//! [`container`](Sandbox::container) shows only those folders, and
//! Landlock (Linux 6.7 or newer) limits TCP to those ports. UDP and Unix
//! sockets are not filtered; [`NoNetwork`] takes away all network from
//! commands.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-sandbox only supports Linux");

mod env;
mod network;
mod profile;
mod protect;

pub use env::command_env;
pub use network::NoNetwork;
pub use profile::Options;
pub use protect::protect_self;

use std::fmt;
use std::path::PathBuf;

use hakoniwa::landlock::{CompatMode, Resource, Ruleset};
use hakoniwa::{Container, Runctl};

/// Folders and network the harness may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    network: Network,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Network {
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
        if let Network::Ports(ports) = &mut self.network {
            ports.push(port);
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
        if let Network::Ports(ports) = &self.network {
            let mut ruleset = Ruleset::default();
            ruleset
                .restrict(Resource::NET_TCP_CONNECT, CompatMode::Enforce)
                .restrict(Resource::NET_TCP_BIND, CompatMode::Enforce);
            for &port in ports {
                ruleset.allow_tcp_connect(port);
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
}
