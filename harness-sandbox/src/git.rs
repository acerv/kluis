// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Git repositories that commands create in the writable folders.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use nix::unistd::Uid;
use rustix::fs::{access, openat, renameat_with, Access, Mode, OFlags, RenameFlags, CWD};
use rustix::io::Errno;
use walkdir::WalkDir;

use crate::mountinfo::{read_mountinfo, remote_points};
use crate::mounts::{checked, path_or_canon_denied, resolve_denied, Hidden};
use crate::{Sandbox, SandboxError};

/// Name a disabled `.git` gets, with a number when it is taken.
const DISABLED: &str = ".git.kl-disabled";

/// Disables git repositories that commands create in the writable folders.
///
/// Git on the host runs commands named by a repository's config, such as
/// `core.fsmonitor`, and its hooks. The sandbox keeps the `.git` of its
/// writable folders read-only, but a command can still create a new one,
/// which the user's next `git status` there, or a shell prompt showing the
/// git status, would run. The guard remembers the repositories there when
/// it starts and renames any new `.git`, so git ignores it.
#[derive(Debug)]
pub struct GitGuard {
    /// Writable folders to scan, without ones inside another, and their
    /// canonical paths.
    roots: Vec<(PathBuf, PathBuf)>,
    /// Every writable path: a `.git` allowed itself is the user's choice.
    writable: Vec<PathBuf>,
    denied: Vec<Hidden>,
    /// Mounts the sandbox hides, as on a network filesystem.
    hidden: Vec<PathBuf>,
    /// The repositories found at the start, and which link each was.
    known: HashMap<PathBuf, Option<LinkId>>,
}

/// What tells a symbolic link from one made in its place: its inode and
/// when it last changed, which no program can set back.
type LinkId = (u64, i64, i64);

/// The [`LinkId`] of `path`, `None` when it is not a symbolic link.
fn link_id(path: &Path) -> Option<LinkId> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.file_type()
        .is_symlink()
        .then(|| (meta.ino(), meta.ctime(), meta.ctime_nsec()))
}

/// What [`GitGuard::disable_new`] found.
#[derive(Debug)]
pub struct GitCheck {
    /// Each new `.git`.
    pub new: Vec<NewRepository>,
    /// The first folder that could not be read, where a command could hide
    /// one.
    pub unchecked: Option<SandboxError>,
}

/// A new `.git` in a writable folder.
#[derive(Debug)]
pub struct NewRepository {
    pub path: PathBuf,
    /// Where it was moved, or why it could not be.
    pub moved_to: io::Result<PathBuf>,
}

impl GitGuard {
    /// Remember the repositories in the writable folders of `sandbox`.
    ///
    /// Fails if a folder there cannot be read, as a repository there could
    /// not be told apart from a new one.
    pub fn new(sandbox: &Sandbox) -> Result<Self, SandboxError> {
        let writable = sandbox
            .write
            .iter()
            .map(|path| checked(path))
            .collect::<Result<Vec<_>, _>>()?;
        // Scan each folder once, not again inside another one: sorted, a
        // folder comes right after the one holding it.
        let mut sorted = writable.clone();
        sorted.sort();
        let mut roots: Vec<(PathBuf, PathBuf)> = Vec::new();
        for path in sorted {
            if roots.last().is_some_and(|(root, _)| path.starts_with(root)) {
                continue;
            }
            let canon = std::fs::canonicalize(&path).map_err(|e| SandboxError::at(&path, e))?;
            roots.push((path, canon));
        }
        let shown = sandbox
            .read
            .iter()
            .map(|path| checked(path))
            .collect::<Result<Vec<_>, _>>()?;
        let allowed: Vec<&Path> = shown
            .iter()
            .chain(&writable)
            .map(PathBuf::as_path)
            .collect();
        let roots_ref: Vec<_> = roots
            .iter()
            .map(|(p, c)| (p.as_path(), c.as_path()))
            .collect();
        let hidden = remote_points(&roots_ref, &allowed, &read_mountinfo()?);

        let mut guard = Self {
            roots,
            writable,
            denied: resolve_denied(&sandbox.deny)?,
            hidden,
            known: HashMap::new(),
        };
        let found = guard.scan().complete()?;
        guard.known = found
            .into_iter()
            .map(|path| {
                let link = link_id(&path);
                (path, link)
            })
            .collect();
        Ok(guard)
    }

    /// Rename each `.git` that appeared since the guard started.
    ///
    /// A `.git` that is a symbolic link in a writable folder itself cannot
    /// be kept read-only: one that is no longer the same link is new too.
    pub fn disable_new(&mut self) -> GitCheck {
        let scan = self.scan();
        let new = scan
            .found
            .into_iter()
            .filter(|path| self.known.get(path) != Some(&link_id(path)))
            .map(|path| {
                let moved_to = disable(&self.roots, &path);
                NewRepository { path, moved_to }
            })
            .collect();
        GitCheck {
            new,
            unchecked: scan.unchecked,
        }
    }

    fn scan(&self) -> GitScan {
        let writable: Vec<&PathBuf> = self.writable.iter().collect();
        let mut scan = GitScan::default();
        for (root, canon) in &self.roots {
            collect_git(
                root,
                canon,
                &writable,
                &self.denied,
                &self.hidden,
                &mut scan,
            );
        }
        scan
    }
}

/// Move `git`, found in one of the `roots`, to a free name next to it that
/// git does not look for. The folders from the root to it are opened
/// without following symbolic links: a command could swap one for a link
/// since the scan, which would lead to a `.git` outside.
fn disable(roots: &[(PathBuf, PathBuf)], git: &Path) -> io::Result<PathBuf> {
    let not_found = || io::Error::new(io::ErrorKind::NotFound, "no longer in the folder");
    let (root, _) = roots
        .iter()
        .find(|(root, _)| git.starts_with(root))
        .ok_or_else(not_found)?;
    let rel = git.strip_prefix(root).map_err(|_| not_found())?;
    let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
        return Err(not_found());
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut dir = openat(CWD, root, flags, Mode::empty())?;
    for folder in parent.components() {
        let flags = flags | OFlags::NOFOLLOW;
        dir = openat(&dir, folder.as_os_str(), flags, Mode::empty())?;
    }
    for n in 1.. {
        let target = match n {
            1 => DISABLED.to_owned(),
            _ => format!("{DISABLED}-{n}"),
        };
        // Never over something there, even if it appears meanwhile.
        match renameat_with(&dir, name, &dir, target.as_str(), RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(git.with_file_name(target)),
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("names run out")
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
/// `writable` themselves, or below `denied` paths or `hidden` mounts,
/// which commands cannot reach. A writable path below a `.git` does not
/// free it: its config and hooks are not what was allowed.
///
/// Commands can make a folder unreadable (`chmod 300`) and still use a
/// `.git` inside it by name, so a folder that cannot be read is reported,
/// not skipped.
pub(crate) fn collect_git(
    root: &Path,
    canon_root: &Path,
    writable: &[&PathBuf],
    denied: &[Hidden],
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
                    let cause = err
                        .io_error()
                        .map_or_else(|| err.to_string(), ToString::to_string);
                    scan.unchecked = Some(SandboxError::at(
                        path,
                        format_args!(
                            "{cause}: a command could hide a .git there; make it \
                             readable or hide it"
                        ),
                    ));
                }
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == ".git" {
            if path.exists() && !writable.iter().any(|w| *w == path) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_follows_no_link_swapped_in() {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("root"), dir.path().join("outside"));
        std::fs::create_dir_all(outside.join(".git")).unwrap();
        std::fs::create_dir(&root).unwrap();
        // A folder that held a new `.git` at the scan, now a link.
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let roots = [(root.clone(), root.clone())];

        assert!(disable(&roots, &root.join("sub/.git")).is_err());
        assert!(outside.join(".git").is_dir());

        std::fs::create_dir_all(root.join("real/.git")).unwrap();
        let moved = disable(&roots, &root.join("real/.git")).unwrap();
        assert_eq!(moved, root.join("real").join(DISABLED));
        assert!(moved.is_dir());
    }
}
