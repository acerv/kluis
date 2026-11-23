// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Filesystem and network sandbox for LLM harnesses, based on hakoniwa
//! containers.
//!
//! A [`Sandbox`] lists the folders the harness may read and write and the
//! TCP ports it may connect to, or allows any network. Its
//! [`container`](Sandbox::container) shows only those folders, and
//! Landlock (Linux 6.7 or newer) limits TCP to those ports. UDP is not
//! filtered; [`NoNetwork`] takes away all network from commands.
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
mod network;
mod profile;
mod protect;
mod seccomp;

pub use env::command_env;
pub use git::{GitCheck, GitGuard, NewRepository};
pub use init::{start_init, stop_others};
pub use network::NoNetwork;
pub use profile::{hidden_secrets, Options};
pub use protect::{kill_first_on_oom, protect_self, refuse_root};
pub use seccomp::{restrict_syscalls, NetworkAccess};

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use hakoniwa::landlock::{CompatMode, Resource, Ruleset};
use hakoniwa::{Container, MountOptions, Namespace, Rlimit, Runctl};
use indexmap::IndexSet;
use nix::unistd::Uid;
use rustix::fs::{access, Access};
use thiserror::Error;
use walkdir::WalkDir;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            return Err(SandboxError {
                message: "max_tmp_size: must be at least 1".to_string(),
            });
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
                return Err(SandboxError {
                    message: "max_processes: must be at least 1".to_string(),
                });
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
        let allowed: Vec<&Path> = all.iter().map(|(path, _)| path.as_path()).collect();
        let mountinfo = read_mountinfo()?;

        // Every `.git` must be found and kept read-only, or commands could
        // change one that git on the host then runs. Hidden mounts are out
        // of their reach.
        let writable_roots: Vec<_> = canon_mounts
            .iter()
            .filter(|(_, _, w)| *w)
            .map(|(root, canon, _)| (root.as_path(), canon.as_path()))
            .collect();
        let remote = remote_points(&writable_roots, &allowed, &mountinfo);
        let mut scan = GitScan::default();
        for (root, canon_root) in &writable_roots {
            collect_git(
                root,
                canon_root,
                &all_writable,
                &denied_targets,
                &remote,
                &mut scan,
            );
        }
        let mut ro_paths = IndexSet::new();
        for git in scan.complete()? {
            resolve_read_only(&canon_mounts, &git, false, &mut ro_paths)?;
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

        let mut mask_paths = map_denied(&canon_mounts, &denied_targets);
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

        // A bind mount brings the mounts of the host below the folder too.
        // On a FUSE or network filesystem, reading, writing or even looking
        // up a file goes to a program or a server that can reach the
        // network, e.g. a cloud drive: hide those, like denied paths,
        // unless allowed themselves.
        let remote = remote_mounts(&canon_mounts, &allowed, &mask_paths, &mountinfo)?;
        for (path, is_dir) in remote {
            mask_paths.push((path.clone(), is_dir));
            effective.push((path, false));
        }

        let mut mounts: IndexSet<_> = canon_mounts
            .into_iter()
            .map(|(path, _, writable)| (path, MountKind::Bind { writable }))
            .collect();
        for path in ro_paths.iter().chain(mask_paths.iter().map(|(p, _)| p)) {
            // The folders on the way from a writable mount become mount
            // points, which cannot be moved away to free the path.
            if let Some((root, true)) = closest_ancestor(&effective, path) {
                for dir in path.ancestors().skip(1).take_while(|d| *d != root) {
                    mounts.insert((dir.to_path_buf(), MountKind::Bind { writable: true }));
                }
            }
        }
        for path in ro_paths {
            // Skip paths already covered by a read-only mount: bind-mounting
            // inside an existing mount touches the target on the host, which
            // fails on files owned by another user (e.g. under /usr).
            if !matches!(closest_ancestor(&effective, &path), Some((_, false))) {
                mounts.insert((path, MountKind::Bind { writable: false }));
            }
        }
        for (path, is_dir) in mask_paths {
            let kind = if is_dir {
                MountKind::MaskDir
            } else {
                MountKind::MaskFile
            };
            mounts.insert((path, kind));
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
pub(crate) fn resolve_denied(deny: &[PathBuf]) -> Result<Vec<(PathBuf, bool)>, SandboxError> {
    let mut out = IndexSet::new();
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
        out.insert((canon, meta.is_dir()));
    }
    Ok(out.into_iter().collect())
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

/// The mount points of `mountinfo` on a filesystem that could reach the
/// network, below one of the `roots` (a mount path and its canonical
/// path), as they show in the container: the sandbox hides them.
///
/// Mounts at or above an `allowed` path stay, as do mounts below another
/// returned one, which hiding already covers.
pub(crate) fn remote_points(
    roots: &[(&Path, &Path)],
    allowed: &[&Path],
    mountinfo: &str,
) -> Vec<PathBuf> {
    // Only the last mount on a path shows: it covers the earlier ones,
    // e.g. a disk mounted over its automount point.
    let top: HashMap<&str, &str> = mountinfo.lines().filter_map(mount_point_and_type).collect();
    let mut found = Vec::new();
    for (point, _) in top.into_iter().filter(|(_, fstype)| !is_local(fstype)) {
        let point = PathBuf::from(unescape(point));
        for (mount, canon) in roots {
            let Some(rest) = point
                .strip_prefix(canon)
                .ok()
                .filter(|r| !r.as_os_str().is_empty())
            else {
                continue;
            };
            let shown = mount.join(rest);
            if !allowed.iter().any(|path| path.starts_with(&shown)) {
                found.push(shown);
            }
        }
    }
    // Sorted, a path comes right before the ones below it.
    found.sort();
    found.dedup_by(|below, above| below.starts_with(above));
    found
}

/// The [`remote_points`] of `mountinfo` below `roots` (a mount path, its
/// canonical path, and whether it is writable) to hide, and whether each
/// is a directory.
///
/// Mounts below a `hidden` path stay, which hiding already covers. So do
/// mounts this user cannot reach, whose program is gone or which deny
/// access: the sandbox, running as the same user, cannot reach them either.
fn remote_mounts(
    roots: &[(PathBuf, PathBuf, bool)],
    allowed: &[&Path],
    hidden: &[(PathBuf, bool)],
    mountinfo: &str,
) -> Result<Vec<(PathBuf, bool)>, SandboxError> {
    let roots: Vec<_> = roots
        .iter()
        .map(|(mount, canon, _)| (mount.as_path(), canon.as_path()))
        .collect();
    let mut out = Vec::new();
    for path in remote_points(&roots, allowed, mountinfo) {
        if hidden.iter().any(|(hidden, _)| path.starts_with(hidden)) {
            continue;
        }
        match std::fs::metadata(&path) {
            Ok(meta) => out.push((path, meta.is_dir())),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(nix::libc::ENOTCONN | nix::libc::EACCES)
                ) => {}
            Err(e) => {
                return Err(SandboxError {
                    message: format!("{}: cannot hide this mount: {e}", path.display()),
                })
            }
        }
    }
    Ok(out)
}

/// The mount point and filesystem type of a `/proc/self/mountinfo` line.
fn mount_point_and_type(line: &str) -> Option<(&str, &str)> {
    // Optional fields end with a lone "-"; spaces in paths are escaped.
    let (before, after) = line.split_once(" - ")?;
    let point = before.split(' ').nth(4)?;
    let fstype = after.split(' ').next()?;
    Some((point, fstype))
}

/// Whether the kernel serves a filesystem of type `fstype` from a local
/// device or from memory, so it cannot reach the network. Any other one,
/// FUSE, NFS, SMB, 9p, autofs or unknown, could.
fn is_local(fstype: &str) -> bool {
    matches!(
        fstype,
        // Disks.
        "ext2" | "ext3" | "ext4" | "btrfs" | "xfs" | "f2fs" | "zfs" | "bcachefs" | "jfs"
            | "reiserfs" | "nilfs2" | "vfat" | "msdos" | "exfat" | "ntfs3" | "hfsplus"
            | "iso9660" | "udf" | "squashfs" | "erofs" | "overlay"
            // Memory and the kernel itself.
            | "tmpfs" | "ramfs" | "proc" | "sysfs" | "devtmpfs" | "devpts" | "mqueue"
            | "cgroup" | "cgroup2" | "securityfs" | "debugfs" | "tracefs" | "pstore"
            | "bpf" | "efivarfs" | "configfs" | "hugetlbfs" | "binfmt_misc" | "selinuxfs"
            | "fusectl" | "nsfs"
    )
}

/// One path of `/proc/self/mountinfo`, where a backslash escapes the octal
/// value of a space, tab, newline or backslash.
fn unescape(field: &str) -> OsString {
    let mut out = Vec::with_capacity(field.len());
    let mut rest = field.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        match tail {
            [a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', after @ ..] if byte == b'\\' => {
                out.push((a - b'0') << 6 | (b - b'0') << 3 | (c - b'0'));
                rest = after;
            }
            _ => {
                out.push(byte);
                rest = tail;
            }
        }
    }
    OsString::from_vec(out)
}

/// The mount table of this namespace.
pub(crate) fn read_mountinfo() -> Result<String, SandboxError> {
    std::fs::read_to_string("/proc/self/mountinfo").map_err(|e| SandboxError {
        message: format!("/proc/self/mountinfo: {e}"),
    })
}

/// Denied targets mapped into every surviving mount that strictly contains
/// them, without entries already inside a denied directory.
fn map_denied(
    canon_mounts: &[(PathBuf, PathBuf, bool)],
    denied: &[(PathBuf, bool)],
) -> Vec<(PathBuf, bool)> {
    let mut mapped = IndexSet::new();
    for (target, is_dir) in denied {
        for (mount, canon, _) in canon_mounts {
            if target != canon {
                if let Ok(rest) = target.strip_prefix(canon) {
                    mapped.insert((mount.join(rest), *is_dir));
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

/// What [`collect_git`] found in the writable folders.
#[derive(Debug, Default)]
pub(crate) struct GitScan {
    /// `.git` folders and files.
    pub found: Vec<PathBuf>,
    /// The first folder it could not look into, where a command could hide
    /// one.
    pub unchecked: Option<SandboxError>,
}

impl GitScan {
    /// The `.git` entries, or an error if a folder could not be checked.
    pub fn complete(self) -> Result<Vec<PathBuf>, SandboxError> {
        match self.unchecked {
            Some(err) => Err(err),
            None => Ok(self.found),
        }
    }
}

/// Add to `scan` the `.git` folders and files inside `root`, unless
/// already covered by `writable`, or below `denied` paths or `hidden`
/// mounts, which commands cannot reach.
///
/// Commands can make a folder unreadable (`chmod 300`) and still use a
/// `.git` inside it by name, so a folder that cannot be read is reported,
/// not skipped.
pub(crate) fn collect_git(
    root: &Path,
    canon_root: &Path,
    writable: &[&PathBuf],
    denied: &[(PathBuf, bool)],
    hidden: &[PathBuf],
    scan: &mut GitScan,
) {
    if !std::fs::metadata(root).is_ok_and(|m| m.is_dir()) {
        return;
    }
    let mut it = WalkDir::new(root).into_iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let path = err.path().unwrap_or(root);
                // The first one is enough to act on: commands could create
                // any number of them.
                if scan.unchecked.is_none() && writable_by_commands(path) {
                    scan.unchecked = Some(SandboxError {
                        message: format!(
                            "{}: {}: a command could hide a .git there; make it \
                             readable or hide it",
                            path.display(),
                            err.io_error()
                                .map_or_else(|| err.to_string(), ToString::to_string)
                        ),
                    });
                }
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == ".git" {
            if path.exists() && !writable.iter().any(|w| w.starts_with(path)) {
                scan.found.push(path.to_path_buf());
            }
            if entry.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        // Real directories (not symlinks) have canonical paths under
        // `canon_root` without needing another syscall.
        if entry.file_type().is_dir()
            && (hidden.iter().any(|mount| mount == path)
                || !denied.is_empty()
                    && path.strip_prefix(root).is_ok_and(|rel| {
                        let canon = canon_root.join(rel);
                        path_or_canon_denied(path, &canon, denied)
                    }))
        {
            it.skip_current_dir();
        }
    }
}

/// Whether commands could have written into `path`, which could not be
/// read. Only a folder of another user that they cannot write to is safe
/// to leave unchecked, such as the data folder of a database container:
/// commands, running as this user, could make one of theirs unreadable.
fn writable_by_commands(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map_or(true, |meta| {
        meta.uid() == Uid::effective().as_raw() || access(path, Access::WRITE_OK).is_ok()
    })
}

/// Resolve `raw` while collecting its target (when inside a writable mount
/// or when `expose_target` is true) and the parents of any symlinks inside
/// writable folders (other than the writable roots themselves) into `ro_paths`.
fn resolve_read_only(
    canon_mounts: &[(PathBuf, PathBuf, bool)],
    raw: &Path,
    expose_target: bool,
    ro_paths: &mut IndexSet<PathBuf>,
) -> Result<(), SandboxError> {
    let abs = checked(raw)?;
    let canonical = walk_symlinks(canon_mounts, &abs, ro_paths)?;
    let canonical = checked(&canonical)?;
    // Protect the path inside every writable mount covering it (mapping back
    // through symlinked mount roots without exposing unmounted host paths).
    for (mount, canon, writable) in canon_mounts {
        if *writable {
            if let Ok(rest) = canonical.strip_prefix(canon) {
                ro_paths.insert(mount.join(rest));
            }
        }
    }
    if expose_target {
        ro_paths.insert(canonical);
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
    ro_paths: &mut IndexSet<PathBuf>,
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
                        ro_paths.insert(checked(&dir)?);
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
pub(crate) fn checked(path: &Path) -> Result<PathBuf, SandboxError> {
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

/// `path` made absolute, as UTF-8.
fn absolute_utf8(path: &Path) -> Result<String, SandboxError> {
    let error = |e: &dyn fmt::Display| SandboxError {
        message: format!("{}: {e}", path.display()),
    };
    std::path::absolute(path)
        .map_err(|e| error(&e))?
        .into_os_string()
        .into_string()
        .map_err(|_| error(&"not UTF-8"))
}
