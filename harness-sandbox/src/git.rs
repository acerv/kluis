// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Git repositories that commands create in the writable folders.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{renameat_with, RenameFlags, CWD};
use rustix::io::Errno;

use crate::mountinfo::{read_mountinfo, remote_points};
use crate::{checked, collect_git, resolve_denied, GitScan, Hidden, Sandbox, SandboxError};

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
    known: HashSet<PathBuf>,
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
            known: HashSet::new(),
        };
        guard.known = guard.scan().complete()?.into_iter().collect();
        Ok(guard)
    }

    /// Rename each `.git` that appeared since the guard started.
    pub fn disable_new(&mut self) -> GitCheck {
        let scan = self.scan();
        let new = scan
            .found
            .into_iter()
            .filter(|path| !self.known.contains(path))
            .map(|path| {
                let moved_to = disable(&path);
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

/// Move `git` to a free name next to it that git does not look for.
fn disable(git: &Path) -> io::Result<PathBuf> {
    for n in 1.. {
        let name = match n {
            1 => DISABLED.to_owned(),
            _ => format!("{DISABLED}-{n}"),
        };
        let target = git.with_file_name(name);
        // Never over something there, even if it appears meanwhile.
        match renameat_with(CWD, git, CWD, &target, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(target),
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("names run out")
}
