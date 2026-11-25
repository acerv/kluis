// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The mounts of a container: what it shows, keeps read-only and hides.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use indexmap::IndexSet;

use crate::git::{collect_git, GitScan};
use crate::mountinfo::{read_mountinfo, remote_points};
use crate::{Sandbox, SandboxError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MountKind {
    Bind { writable: bool },
    MaskDir,
    MaskFile,
}

/// A folder or file the container shows.
#[derive(Debug, Clone)]
struct Mount {
    /// Where it is, as it was allowed.
    path: PathBuf,
    /// The same without symlinks.
    canon: PathBuf,
    writable: bool,
}

/// A path the container hides, masked as the folder or the file it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Hidden {
    pub path: PathBuf,
    pub is_dir: bool,
}

impl Sandbox {
    /// Absolute paths to mount and how to mount them, without the ones
    /// already shown by a mount of a parent folder, plus the `.git` of
    /// writable folders and the paths to keep read-only as read-only, and
    /// any denied paths masked over them.
    pub(crate) fn mounts(&self) -> Result<Vec<(String, MountKind)>, SandboxError> {
        let mut all = Vec::new();
        for (paths, writable) in [(&self.read, false), (&self.write, true)] {
            for path in paths {
                all.push((checked(path)?, writable));
            }
        }
        let denied = resolve_denied(&self.deny)?;
        let mut shown = shown_mounts(&all, &denied)?;
        let allowed: Vec<&Path> = all.iter().map(|(path, _)| path.as_path()).collect();
        let mountinfo = read_mountinfo()?;

        let mut ro_paths = IndexSet::new();
        for git in git_entries(&shown, &all, &allowed, &denied, &mountinfo)? {
            resolve_read_only(&shown, &git, false, &mut ro_paths)?;
        }
        for path in &self.read_only {
            resolve_read_only(&shown, path, true, &mut ro_paths)?;
        }
        ro_paths.retain(|p| !covered_by_denied(p, &denied));

        let mut masks = map_denied(&shown, &denied);
        ro_paths.retain(|ro| !masks.iter().any(|m| ro.starts_with(&m.path)));

        // A bind mount brings the mounts of the host below the folder too.
        // On a FUSE or network filesystem, reading, writing or even looking
        // up a file goes to a program or a server that can reach the
        // network, e.g. a cloud drive: hide those, like denied paths,
        // unless allowed themselves.
        let remote = remote_mounts(&shown, &allowed, &masks, &mountinfo)?;
        masks.extend(remote);

        // A path kept read-only also overrides writable mounts at or below
        // it. Only now: what they show still needs its masks.
        shown.retain(|m| {
            !m.writable
                || !ro_paths
                    .iter()
                    .any(|ro| m.path.starts_with(ro) || m.canon.starts_with(ro))
        });

        ordered(shown, ro_paths, masks)
            .into_iter()
            .map(|(path, kind)| {
                let path = path
                    .into_os_string()
                    .into_string()
                    .map_err(|p| SandboxError::at(Path::new(&p), "not UTF-8"))?;
                Ok((path, kind))
            })
            .collect()
    }
}

/// The mounts for `all` the allowed paths and whether each is writable,
/// without the ones a mount of a parent folder already shows, and the
/// ones at or below a `denied` path.
fn shown_mounts(all: &[(PathBuf, bool)], denied: &[Hidden]) -> Result<Vec<Mount>, SandboxError> {
    // A mount inside another one would create its mount point on the
    // host; a writable mount of the same path also supersedes a
    // read-only one.
    let covered = |(path, writable): &(PathBuf, bool)| {
        all.iter().any(|(other, w)| {
            (other != path && path.starts_with(other) && (*w || !writable))
                || (other == path && *w && !writable)
        })
    };
    let mut shown = Vec::new();
    for (mount, writable) in all.iter().filter(|m| !covered(m)) {
        if shown
            .iter()
            .any(|m: &Mount| m.path == *mount && m.writable == *writable)
        {
            continue;
        }
        let canon = std::fs::canonicalize(mount).map_err(|e| SandboxError::at(mount, e))?;
        if !path_or_canon_denied(mount, &canon, denied) {
            shown.push(Mount {
                path: mount.clone(),
                canon,
                writable: *writable,
            });
        }
    }
    Ok(shown)
}

/// Every `.git` in the writable mounts of `shown`: all must be found and
/// kept read-only, or commands could change one that git on the host then
/// runs. Hidden mounts are out of their reach.
fn git_entries(
    shown: &[Mount],
    all: &[(PathBuf, bool)],
    allowed: &[&Path],
    denied: &[Hidden],
    mountinfo: &str,
) -> Result<Vec<PathBuf>, SandboxError> {
    let all_writable: Vec<_> = all.iter().filter(|(_, w)| *w).map(|(p, _)| p).collect();
    let writable_roots: Vec<_> = shown
        .iter()
        .filter(|m| m.writable)
        .map(|m| (m.path.as_path(), m.canon.as_path()))
        .collect();
    let remote = remote_points(&writable_roots, allowed, mountinfo);
    let mut scan = GitScan::default();
    for (root, canon_root) in &writable_roots {
        collect_git(root, canon_root, &all_writable, denied, &remote, &mut scan);
    }
    scan.complete()
}

/// The mounts in the order to make them: the ones `shown`, the folders
/// leading to the next ones, the paths kept read-only, then the `masks`.
fn ordered(
    shown: Vec<Mount>,
    ro_paths: IndexSet<PathBuf>,
    masks: Vec<Hidden>,
) -> IndexSet<(PathBuf, MountKind)> {
    // Every mount and whether it is writable.
    let effective: Vec<_> = shown
        .iter()
        .map(|m| (m.path.clone(), m.writable))
        .chain(ro_paths.iter().map(|path| (path.clone(), false)))
        .chain(masks.iter().map(|m| (m.path.clone(), false)))
        .collect();
    let mut mounts: IndexSet<_> = shown
        .into_iter()
        .map(|m| {
            let writable = m.writable;
            (m.path, MountKind::Bind { writable })
        })
        .collect();
    for path in ro_paths.iter().chain(masks.iter().map(|m| &m.path)) {
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
    for Hidden { path, is_dir } in masks {
        let kind = if is_dir {
            MountKind::MaskDir
        } else {
            MountKind::MaskFile
        };
        mounts.insert((path, kind));
    }
    mounts
}

/// Canonical paths and whether they are directories for existing `deny` entries.
pub(crate) fn resolve_denied(deny: &[PathBuf]) -> Result<Vec<Hidden>, SandboxError> {
    let mut out = IndexSet::new();
    for raw in deny {
        let error = |e: &dyn fmt::Display| SandboxError::at(raw, e);
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
        out.insert(Hidden {
            path: canon,
            is_dir: meta.is_dir(),
        });
    }
    Ok(out.into_iter().collect())
}

/// Whether `path` or its canonical target `canon` is at or below any denied target.
pub(crate) fn path_or_canon_denied(path: &Path, canon: &Path, denied: &[Hidden]) -> bool {
    denied
        .iter()
        .any(|d| path.starts_with(&d.path) || canon.starts_with(&d.path))
}

/// Whether `path` (or its canonical target) is at or below any denied target.
fn covered_by_denied(path: &Path, denied: &[Hidden]) -> bool {
    if denied.is_empty() {
        return false;
    }
    let canon = std::fs::canonicalize(path).ok();
    denied.iter().any(|d| {
        path.starts_with(&d.path) || canon.as_deref().is_some_and(|c| c.starts_with(&d.path))
    })
}

/// The [`remote_points`] of `mountinfo` below `roots` (a mount path, its
/// canonical path, and whether it is writable) to hide, and whether each
/// is a directory.
///
/// Mounts below a `hidden` path stay, which hiding already covers. So do
/// mounts this user cannot reach, whose program is gone, which deny access
/// or whose path is gone, as below another mount: the sandbox, running as
/// the same user, cannot reach them either.
fn remote_mounts(
    roots: &[Mount],
    allowed: &[&Path],
    hidden: &[Hidden],
    mountinfo: &str,
) -> Result<Vec<Hidden>, SandboxError> {
    let roots: Vec<_> = roots
        .iter()
        .map(|m| (m.path.as_path(), m.canon.as_path()))
        .collect();
    let mut out = Vec::new();
    for path in remote_points(&roots, allowed, mountinfo) {
        if hidden.iter().any(|hidden| path.starts_with(&hidden.path)) {
            continue;
        }
        match std::fs::metadata(&path) {
            Ok(meta) => out.push(Hidden {
                path,
                is_dir: meta.is_dir(),
            }),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(nix::libc::ENOTCONN | nix::libc::EACCES | nix::libc::ENOENT)
                ) => {}
            Err(e) => {
                return Err(SandboxError::at(
                    &path,
                    format_args!("cannot hide this mount: {e}"),
                ))
            }
        }
    }
    Ok(out)
}

/// Denied targets mapped into every surviving mount that strictly contains
/// them, without entries already inside a denied directory.
fn map_denied(canon_mounts: &[Mount], denied: &[Hidden]) -> Vec<Hidden> {
    let mut mapped = IndexSet::new();
    for Hidden {
        path: target,
        is_dir,
    } in denied
    {
        for mount in canon_mounts {
            if *target != mount.canon {
                if let Ok(rest) = target.strip_prefix(&mount.canon) {
                    mapped.insert(Hidden {
                        path: mount.path.join(rest),
                        is_dir: *is_dir,
                    });
                }
            }
        }
    }
    mapped
        .iter()
        .filter(|hidden| {
            !mapped.iter().any(|other| {
                other.is_dir && other != *hidden && hidden.path.starts_with(&other.path)
            })
        })
        .cloned()
        .collect()
}

/// Resolve `raw` while collecting its target (when inside a writable mount
/// or when `expose_target` is true) and the parents of any symlinks inside
/// writable folders (other than the writable roots themselves) into `ro_paths`.
fn resolve_read_only(
    canon_mounts: &[Mount],
    raw: &Path,
    expose_target: bool,
    ro_paths: &mut IndexSet<PathBuf>,
) -> Result<(), SandboxError> {
    let abs = checked(raw)?;
    let canonical = walk_symlinks(canon_mounts, &abs, ro_paths)?;
    let canonical = checked(&canonical)?;
    // Protect the path inside every writable mount covering it (mapping back
    // through symlinked mount roots without exposing unmounted host paths).
    for mount in canon_mounts.iter().filter(|m| m.writable) {
        if let Ok(rest) = canonical.strip_prefix(&mount.canon) {
            // Not `join` for the mount itself: it adds a trailing slash,
            // which a file does not take.
            let path = if rest.as_os_str().is_empty() {
                mount.path.clone()
            } else {
                mount.path.join(rest)
            };
            ro_paths.insert(path);
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
    canon_mounts: &[Mount],
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
                let meta =
                    std::fs::symlink_metadata(&next).map_err(|e| SandboxError::at(&next, e))?;
                if meta.file_type().is_symlink() {
                    hops += 1;
                    if hops > MAX_SYMLINKS {
                        return Err(SandboxError::at(abs, "too many levels of symbolic links"));
                    }
                    if let Some(dir) = writable_symlink_parent(canon_mounts, &cur) {
                        ro_paths.insert(checked(&dir)?);
                    }
                    let target =
                        std::fs::read_link(&next).map_err(|e| SandboxError::at(&next, e))?;
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
fn writable_symlink_parent(canon_mounts: &[Mount], dir: &Path) -> Option<PathBuf> {
    let mount = canon_mounts
        .iter()
        .filter(|m| dir.starts_with(&m.canon))
        .max_by_key(|m| m.canon.components().count())
        .filter(|m| m.writable)?;
    let rest = dir.strip_prefix(&mount.canon).ok()?;
    if rest.as_os_str().is_empty() {
        // The symlink sits directly inside a writable root; making the
        // whole root read-only would break `allow_write`.
        return None;
    }
    Some(mount.path.join(rest))
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
    let error = |e: &dyn fmt::Display| SandboxError::at(path, e);
    let path = std::path::absolute(path).map_err(|e| error(&e))?;
    std::fs::metadata(&path).map_err(|e| error(&e))?;
    if path.to_str().is_none() {
        return Err(error(&"not UTF-8"));
    }
    Ok(path)
}
