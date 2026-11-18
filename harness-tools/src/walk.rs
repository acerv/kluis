// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Walking a directory tree the way `find` and `grep` share.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ignore::WalkBuilder;

/// A walker below `root`: hidden files are included, `.git` is not, and
/// files ignored by `.gitignore`, `.ignore` or git's excludes are left
/// out, even outside a git repository. Symlinks are not followed.
pub(crate) fn builder(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        // Like git, let a nested repository stop the .gitignore rules of the
        // outer one; outside any repository, apply them anyway.
        .require_git(in_git_repo(root))
        .current_dir(root)
        .filter_entry(|entry| entry.file_name() != ".git");
    builder
}

/// Whether `dir` is inside a git work tree.
fn in_git_repo(dir: &Path) -> bool {
    let Ok(dir) = std::path::absolute(dir) else {
        return false;
    };
    dir.ancestors().any(|d| d.join(".git").exists())
}

/// Raises its flag when dropped, so blocking work started by a future stops
/// when the future is dropped.
#[derive(Default)]
pub(crate) struct StopOnDrop(pub(crate) Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
