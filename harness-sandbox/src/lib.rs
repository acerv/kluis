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

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use hakoniwa::landlock::{CompatMode, Resource, Ruleset};
use hakoniwa::{Container, MountOptions, Rlimit, Runctl};

/// Folders and network the harness may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    read_only: Vec<PathBuf>,
    network: Network,
    max_processes: Option<u64>,
    max_tmp_size: Option<u64>,
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

    /// Allow reading, executing and changing anything below `path`, but
    /// any `.git` below it, which stays read-only unless allowed itself:
    /// git on the host would otherwise run the hooks and commands written
    /// there.
    pub fn allow_write(mut self, path: impl Into<PathBuf>) -> Self {
        self.write.push(path.into());
        self
    }

    /// Allow reading and executing anything below `path`, and keep it
    /// read-only even below, or at, a writable path. The folders between
    /// that writable path and `path` can then no longer be moved or
    /// removed, which would let a new file take its place.
    pub fn keep_read_only(mut self, path: impl Into<PathBuf>) -> Self {
        self.read_only.push(path.into());
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

    /// Let the processes and threads of the current user number at most
    /// `max` in the container, so a fork bomb cannot fill the host.
    pub fn max_processes(mut self, max: u64) -> Self {
        self.max_processes = Some(max);
        self
    }

    /// Let the private `/tmp` hold at most `bytes`; it lives in memory.
    pub fn max_tmp_size(mut self, bytes: u64) -> Self {
        self.max_tmp_size = Some(bytes);
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
            .mount(
                "tmpfs",
                "/tmp",
                "tmpfs",
                MountOptions::NOSUID | MountOptions::NODEV,
                self.max_tmp_size.map(|bytes| format!("size={bytes}")),
            );
        for (path, writable) in self.mounts()? {
            if writable {
                container.bindmount_rw(&path, &path);
            } else {
                container.bindmount_ro(&path, &path);
            }
        }
        if let Some(max) = self.max_processes {
            container.setrlimit(Rlimit::Nproc, max, max);
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
    /// ones already shown by a mount of a parent folder, plus the `.git` of
    /// writable folders and the paths to keep read-only as read-only.
    fn mounts(&self) -> Result<Vec<(String, bool)>, SandboxError> {
        let mut all = Vec::new();
        for (paths, writable) in [(&self.read, false), (&self.write, true)] {
            for path in paths {
                all.push((checked(path)?, writable));
            }
        }
        // A mount inside another one would create its mount point on the
        // host.
        let covered = |(path, writable): &(PathBuf, bool)| {
            all.iter()
                .any(|(other, w)| other != path && path.starts_with(other) && (*w || !writable))
        };
        let base_mounts: Vec<_> = all.iter().filter(|m| !covered(m)).cloned().collect();
        let all_writable: Vec<_> = all.iter().filter(|(_, w)| *w).map(|(p, _)| p).collect();
        let writable_roots: Vec<_> = base_mounts
            .iter()
            .filter(|(_, w)| *w)
            .map(|(p, _)| p.clone())
            .collect();
        let canon_mounts: Vec<_> = base_mounts
            .iter()
            .filter_map(|(m, w)| Some((m.clone(), std::fs::canonicalize(m).ok()?, *w)))
            .collect();

        let mut ro_paths = Vec::new();
        for root in &writable_roots {
            for git in collect_git(root, &all_writable) {
                // Skip `.git` entries that vanished or cannot be mounted
                // (e.g. non-UTF-8 paths) instead of failing startup.
                let mut found = Vec::new();
                if resolve_read_only(&canon_mounts, &git, false, &mut found).is_ok() {
                    for path in found {
                        push_unique(&mut ro_paths, path);
                    }
                }
            }
        }
        for path in &self.read_only {
            resolve_read_only(&canon_mounts, path, true, &mut ro_paths)?;
        }

        let mut effective = base_mounts.clone();
        for path in &ro_paths {
            effective.push((path.clone(), false));
        }

        let mut mounts = base_mounts;
        for path in &ro_paths {
            // The folders on the way from a writable mount become mount
            // points, which cannot be moved away to free the path.
            if let Some((root, true)) = closest_ancestor(&effective, path) {
                for dir in path.ancestors().skip(1).take_while(|d| *d != root) {
                    mounts.push((dir.to_path_buf(), true));
                }
            }
        }
        for path in ro_paths {
            // Skip paths already covered by a read-only mount: bind-mounting
            // inside an existing mount touches the target on the host, which
            // fails on files owned by another user (e.g. under /usr).
            if !matches!(closest_ancestor(&effective, &path), Some((_, false))) {
                mounts.push((path, false));
            }
        }
        mounts
            .into_iter()
            .map(|(path, writable)| {
                let path = path
                    .into_os_string()
                    .into_string()
                    .map_err(|p| SandboxError {
                        message: format!("{}: not UTF-8", PathBuf::from(p).display()),
                    })?;
                Ok((path, writable))
            })
            .collect()
    }
}

/// `.git` folders and files inside `root`, unless already covered by `writable`.
fn collect_git(root: &Path, writable: &[&PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(meta) = std::fs::metadata(root) else {
        return found;
    };
    if !meta.is_dir() {
        return found;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name() == ".git" {
                if path.exists() && !writable.iter().any(|w| w.starts_with(&path)) {
                    found.push(path);
                }
                continue;
            }
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                stack.push(path);
            }
        }
    }
    found
}

/// Resolve `raw` while collecting its target (when inside a writable mount
/// or when `expose_target` is true) and the parents of any symlinks inside
/// writable folders (other than the writable roots themselves) into `ro_paths`.
fn resolve_read_only(
    canon_mounts: &[(PathBuf, PathBuf, bool)],
    raw: &Path,
    expose_target: bool,
    ro_paths: &mut Vec<PathBuf>,
) -> Result<(), SandboxError> {
    let abs = checked(raw)?;
    let canonical = walk_symlinks(canon_mounts, &abs, ro_paths)?;
    let canonical = checked(&canonical)?;
    // Protect the path inside every writable mount covering it (mapping back
    // through symlinked mount roots without exposing unmounted host paths).
    for (mount, canon, writable) in canon_mounts {
        if *writable {
            if let Ok(rest) = canonical.strip_prefix(canon) {
                push_unique(ro_paths, mount.join(rest));
            }
        }
    }
    if expose_target {
        push_unique(ro_paths, canonical);
    }
    Ok(())
}

enum Step {
    Root,
    Parent,
    Name(OsString),
}

fn push_steps(queue: &mut VecDeque<Step>, path: &Path) {
    let steps: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            Component::RootDir => Some(Step::Root),
            Component::ParentDir => Some(Step::Parent),
            Component::Normal(name) => Some(Step::Name(name.to_owned())),
            Component::CurDir | Component::Prefix(_) => None,
        })
        .collect();
    for step in steps.into_iter().rev() {
        queue.push_front(step);
    }
}

/// Walk `abs` component by component, following symlinks and recording the
/// writable parent directory of any symlink strictly below a writable root.
fn walk_symlinks(
    canon_mounts: &[(PathBuf, PathBuf, bool)],
    abs: &Path,
    ro_paths: &mut Vec<PathBuf>,
) -> Result<PathBuf, SandboxError> {
    const MAX_SYMLINKS: usize = 40;
    let mut cur = PathBuf::from("/");
    let mut queue = VecDeque::new();
    push_steps(&mut queue, abs);
    let mut hops = 0usize;

    while let Some(step) = queue.pop_front() {
        match step {
            Step::Root => cur = PathBuf::from("/"),
            Step::Parent => {
                cur.pop();
            }
            Step::Name(name) => {
                let next = cur.join(&name);
                let meta = std::fs::symlink_metadata(&next).map_err(|e| SandboxError {
                    message: format!("{}: {e}", next.display()),
                })?;
                if meta.file_type().is_symlink() {
                    hops += 1;
                    if hops > MAX_SYMLINKS {
                        return Err(SandboxError {
                            message: format!(
                                "{}: too many levels of symbolic links",
                                abs.display()
                            ),
                        });
                    }
                    if let Some(dir) = writable_symlink_parent(canon_mounts, &cur) {
                        push_unique(ro_paths, checked(&dir)?);
                    }
                    let target = std::fs::read_link(&next).map_err(|e| SandboxError {
                        message: format!("{}: {e}", next.display()),
                    })?;
                    push_steps(&mut queue, &target);
                } else {
                    cur = next;
                }
            }
        }
    }
    Ok(cur)
}

/// If `dir` is strictly inside a writable mount, return its path relative
/// to that mount so it can be kept read-only.
fn writable_symlink_parent(
    canon_mounts: &[(PathBuf, PathBuf, bool)],
    dir: &Path,
) -> Option<PathBuf> {
    let (mount, canon, _) = canon_mounts
        .iter()
        .filter(|(_, canon, _)| dir.starts_with(canon))
        .max_by_key(|(_, canon, _)| canon.components().count())
        .filter(|(_, _, writable)| *writable)?;
    let rest = dir.strip_prefix(canon).ok()?;
    if rest.as_os_str().is_empty() {
        // The symlink sits directly inside a writable root; making the
        // whole root read-only would break `allow_write`.
        return None;
    }
    Some(mount.join(rest))
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// The closest mount strictly above `path` and whether it is writable.
fn closest_ancestor(mounts: &[(PathBuf, bool)], path: &Path) -> Option<(PathBuf, bool)> {
    mounts
        .iter()
        .filter(|(mount, _)| mount != path && path.starts_with(mount))
        .max_by_key(|(mount, _)| mount.components().count())
        .cloned()
}

/// `path` made absolute, if it exists and is UTF-8.
fn checked(path: &Path) -> Result<PathBuf, SandboxError> {
    let error = |e: &dyn fmt::Display| SandboxError {
        message: format!("{}: {e}", path.display()),
    };
    let path = std::path::absolute(path).map_err(|e| error(&e))?;
    std::fs::metadata(&path).map_err(|e| error(&e))?;
    if path.to_str().is_none() {
        return Err(error(&"not UTF-8"));
    }
    Ok(path)
}
