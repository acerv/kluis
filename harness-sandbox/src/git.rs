// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Git repositories that commands create in the writable folders.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{renameat_with, RenameFlags, CWD};
use rustix::io::Errno;

use crate::{checked, collect_git, resolve_denied, Sandbox, SandboxError};

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
    denied: Vec<(PathBuf, bool)>,
    known: HashSet<PathBuf>,
}

/// A `.git` that [`GitGuard::disable_new`] found.
#[derive(Debug)]
pub struct NewRepository {
    pub path: PathBuf,
    /// Where it was moved, or why it could not be.
    pub moved_to: io::Result<PathBuf>,
}

impl GitGuard {
    /// Remember the repositories in the writable folders of `sandbox`.
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
            let canon = std::fs::canonicalize(&path).map_err(|e| SandboxError {
                message: format!("{}: {e}", path.display()),
            })?;
            roots.push((path, canon));
        }

        let mut guard = Self {
            roots,
            writable,
            denied: resolve_denied(&sandbox.deny)?,
            known: HashSet::new(),
        };
        guard.known = guard.scan().into_iter().collect();
        Ok(guard)
    }

    /// Rename each `.git` that appeared since the guard started.
    pub fn disable_new(&mut self) -> Vec<NewRepository> {
        self.scan()
            .into_iter()
            .filter(|path| !self.known.contains(path))
            .map(|path| {
                let moved_to = disable(&path);
                NewRepository { path, moved_to }
            })
            .collect()
    }

    fn scan(&self) -> Vec<PathBuf> {
        let writable: Vec<&PathBuf> = self.writable.iter().collect();
        self.roots
            .iter()
            .flat_map(|(root, canon)| collect_git(root, canon, &writable, &self.denied))
            .collect()
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
