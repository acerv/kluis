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
use std::io;
use std::path::{Component, Path, PathBuf};

use hakoniwa::landlock::{CompatMode, Resource, Ruleset};
use hakoniwa::{Container, MountOptions, Rlimit, Runctl};
use thiserror::Error;
use walkdir::WalkDir;

/// Folders and network the harness may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    read_only: Vec<PathBuf>,
    deny: Vec<PathBuf>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountKind {
    Bind { writable: bool },
    MaskDir,
    MaskFile,
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
        if self.max_tmp_size == Some(0) {
            return Err(SandboxError {
                message: "max_tmp_size: must be at least 1".to_string(),
            });
        }
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
        if let Some(max) = self.max_processes {
            if max == 0 {
                return Err(SandboxError {
                    message: "max_processes: must be at least 1".to_string(),
                });
            }
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

    /// Absolute paths to mount and how to mount them, without the ones
    /// already shown by a mount of a parent folder, plus the `.git` of
    /// writable folders and the paths to keep read-only as read-only, and
    /// any denied paths masked over them.
    fn mounts(&self) -> Result<Vec<(String, MountKind)>, SandboxError> {
        let mut all = Vec::new();
        for (paths, writable) in [(&self.read, false), (&self.write, true)] {
            for path in paths {
                all.push((checked(path)?, writable));
            }
        }
        let denied_targets = resolve_denied(&self.deny)?;

        // A mount inside another one would create its mount point on the
        // host; a writable mount of the same path also supersedes a
        // read-only one.
        let covered = |(path, writable): &(PathBuf, bool)| {
            all.iter().any(|(other, w)| {
                (other != path && path.starts_with(other) && (*w || !writable))
                    || (other == path && *w && !writable)
            })
        };
        let mut canon_mounts = Vec::new();
        for (mount, writable) in all.iter().filter(|m| !covered(m)) {
            if canon_mounts
                .iter()
                .any(|(m, _, w)| m == mount && w == writable)
            {
                continue;
            }
            let canon = std::fs::canonicalize(mount).map_err(|e| SandboxError {
                message: format!("{}: {e}", mount.display()),
            })?;
            if !path_or_canon_denied(mount, &canon, &denied_targets) {
                canon_mounts.push((mount.clone(), canon, *writable));
            }
        }
        let all_writable: Vec<_> = all.iter().filter(|(_, w)| *w).map(|(p, _)| p).collect();

        let mut ro_paths = Vec::new();
        for (root, canon_root, writable) in &canon_mounts {
            if !*writable {
                continue;
            }
            for git in collect_git(root, canon_root, &all_writable, &denied_targets) {
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
        ro_paths.retain(|p| !covered_by_denied(p, &denied_targets));

        // A path kept read-only also overrides writable mounts at or below it.
        canon_mounts.retain(|(path, canon, writable)| {
            !*writable
                || !ro_paths
                    .iter()
                    .any(|ro| path.starts_with(ro) || canon.starts_with(ro))
        });

        let mask_paths = map_denied(&canon_mounts, &denied_targets);
        ro_paths.retain(|ro| !mask_paths.iter().any(|(d, _)| ro.starts_with(d)));

        let mut effective: Vec<_> = canon_mounts
            .iter()
            .map(|(m, _, w)| (m.clone(), *w))
            .collect();
        for path in &ro_paths {
            effective.push((path.clone(), false));
        }
        for (path, _) in &mask_paths {
            effective.push((path.clone(), false));
        }

        let mut mounts: Vec<_> = canon_mounts
            .into_iter()
            .map(|(path, _, writable)| (path, MountKind::Bind { writable }))
            .collect();
        for path in ro_paths.iter().chain(mask_paths.iter().map(|(p, _)| p)) {
            // The folders on the way from a writable mount become mount
            // points, which cannot be moved away to free the path.
            if let Some((root, true)) = closest_ancestor(&effective, path) {
                for dir in path.ancestors().skip(1).take_while(|d| *d != root) {
                    let pin = (dir.to_path_buf(), MountKind::Bind { writable: true });
                    if !mounts.contains(&pin) {
                        mounts.push(pin);
                    }
                }
            }
        }
        for path in ro_paths {
            // Skip paths already covered by a read-only mount: bind-mounting
            // inside an existing mount touches the target on the host, which
            // fails on files owned by another user (e.g. under /usr).
            if !matches!(closest_ancestor(&effective, &path), Some((_, false))) {
                mounts.push((path, MountKind::Bind { writable: false }));
            }
        }
        for (path, is_dir) in mask_paths {
            let kind = if is_dir {
                MountKind::MaskDir
            } else {
                MountKind::MaskFile
            };
            mounts.push((path, kind));
        }
        mounts
            .into_iter()
            .map(|(path, kind)| {
                let path = path
                    .into_os_string()
                    .into_string()
                    .map_err(|p| SandboxError {
                        message: format!("{}: not UTF-8", PathBuf::from(p).display()),
                    })?;
                Ok((path, kind))
            })
            .collect()
    }
}

/// Canonical paths and whether they are directories for existing `deny` entries.
fn resolve_denied(deny: &[PathBuf]) -> Result<Vec<(PathBuf, bool)>, SandboxError> {
    let mut out = Vec::new();
    for raw in deny {
        let error = |e: &dyn fmt::Display| SandboxError {
            message: format!("{}: {e}", raw.display()),
        };
        let abs = std::path::absolute(raw).map_err(|e| error(&e))?;
        if abs.to_str().is_none() {
            return Err(error(&"not UTF-8"));
        }
        let meta = match std::fs::metadata(&abs) {
            Ok(meta) => meta,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(e) => return Err(error(&e)),
        };
        let canon = std::fs::canonicalize(&abs).map_err(|e| error(&e))?;
        if canon.to_str().is_none() {
            return Err(error(&"not UTF-8"));
        }
        let entry = (canon, meta.is_dir());
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Whether `path` or its canonical target `canon` is at or below any denied target.
fn path_or_canon_denied(path: &Path, canon: &Path, denied: &[(PathBuf, bool)]) -> bool {
    denied
        .iter()
        .any(|(d, _)| path.starts_with(d) || canon.starts_with(d))
}

/// Whether `path` (or its canonical target) is at or below any denied target.
fn covered_by_denied(path: &Path, denied: &[(PathBuf, bool)]) -> bool {
    if denied.is_empty() {
        return false;
    }
    let canon = std::fs::canonicalize(path).ok();
    denied
        .iter()
        .any(|(d, _)| path.starts_with(d) || canon.as_deref().is_some_and(|c| c.starts_with(d)))
}

/// Denied targets mapped into every surviving mount that strictly contains
/// them, without entries already inside a denied directory.
fn map_denied(
    canon_mounts: &[(PathBuf, PathBuf, bool)],
    denied: &[(PathBuf, bool)],
) -> Vec<(PathBuf, bool)> {
    let mut mapped = Vec::new();
    for (target, is_dir) in denied {
        for (mount, canon, _) in canon_mounts {
            if target != canon {
                if let Ok(rest) = target.strip_prefix(canon) {
                    let entry = (mount.join(rest), *is_dir);
                    if !mapped.contains(&entry) {
                        mapped.push(entry);
                    }
                }
            }
        }
    }
    mapped
        .iter()
        .filter(|(path, _)| {
            !mapped
                .iter()
                .any(|(other, is_dir)| *is_dir && other != path && path.starts_with(other))
        })
        .cloned()
        .collect()
}

/// `.git` folders and files inside `root`, unless already covered by
/// `writable` or `denied`.
fn collect_git(
    root: &Path,
    canon_root: &Path,
    writable: &[&PathBuf],
    denied: &[(PathBuf, bool)],
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if !std::fs::metadata(root).is_ok_and(|m| m.is_dir()) {
        return found;
    }
    let mut it = WalkDir::new(root).into_iter();
    while let Some(entry) = it.next() {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == ".git" {
            if path.exists() && !writable.iter().any(|w| w.starts_with(path)) {
                found.push(path.to_path_buf());
            }
            if entry.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        // Real directories (not symlinks) have canonical paths under
        // `canon_root` without needing another syscall.
        if entry.file_type().is_dir()
            && !denied.is_empty()
            && path.strip_prefix(root).is_ok_and(|rel| {
                let canon = canon_root.join(rel);
                path_or_canon_denied(path, &canon, denied)
            })
        {
            it.skip_current_dir();
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

/// Limit the virtual memory of the calling process and its children to
/// `bytes`, e.g. a command before `exec`.
///
/// Only makes a system call, so it is safe to use in `Command::pre_exec`.
pub fn limit_memory(bytes: u64) -> io::Result<()> {
    nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_AS, bytes, bytes)
        .map_err(io::Error::from)
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
