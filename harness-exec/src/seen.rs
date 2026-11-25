// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What the model last saw of the files it read or wrote, so `edit` and
//! `write` do not overwrite a change made since by someone else.

use std::collections::HashMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::ToolError;
use crate::lock;
use crate::path::resolve;

/// Most files remembered. Past it all are forgotten: the model is asked
/// nothing more, and files it reads again are remembered again.
pub const MAX_SEEN_FILES: usize = 4096;

/// What tells one version of a file from the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
}

impl Stamp {
    fn of(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        }
    }
}

/// A file as it was when looked at.
pub(crate) struct Look {
    key: PathBuf,
    stamp: Option<Stamp>,
}

/// The files the model read or wrote, as they were then.
#[derive(Debug, Default)]
pub struct SeenFiles(Mutex<HashMap<PathBuf, Stamp>>);

impl SeenFiles {
    /// `path`, relative to `cwd`, as it is now, to [`Self::remember`] once
    /// the model saw it: looked at after it is read, a change made
    /// meanwhile would pass for seen.
    pub(crate) fn look(cwd: &Path, path: &str) -> Look {
        let (key, meta) = locate(cwd, path);
        Look {
            key,
            stamp: meta.as_ref().map(Stamp::of),
        }
    }

    /// Remember the file of `look` as it was then. A file that could not
    /// be looked at is forgotten.
    pub(crate) fn remember(&self, look: Look) {
        let Look { key, stamp } = look;
        let mut seen = lock(&self.0);
        match stamp {
            Some(stamp) => {
                if seen.len() >= MAX_SEEN_FILES && !seen.contains_key(&key) {
                    seen.clear();
                }
                seen.insert(key, stamp);
            }
            None => {
                seen.remove(&key);
            }
        }
    }

    /// Remember `path`, relative to `cwd`, as it is now.
    pub(crate) fn record(&self, cwd: &Path, path: &str) {
        self.remember(Self::look(cwd, path));
    }

    /// Fail when `path`, relative to `cwd`, changed since it was
    /// remembered. A file never remembered passes, and so does one that is
    /// gone: nothing is left to overwrite.
    pub(crate) fn check(&self, cwd: &Path, path: &str) -> Result<(), ToolError> {
        let (key, meta) = locate(cwd, path);
        let seen = lock(&self.0);
        match (seen.get(&key), meta) {
            (Some(then), Some(meta)) if Stamp::of(&meta) != *then => Err(ToolError::new(format!(
                "{path}: changed since you last read it. Read it again before changing it"
            ))),
            _ => Ok(()),
        }
    }
}

/// The key of `path` and what is there now. Symlinks are followed, so every
/// name of a file shares one key.
fn locate(cwd: &Path, path: &str) -> (PathBuf, Option<Metadata>) {
    let path = resolve(cwd, path);
    let key = std::fs::canonicalize(&path).unwrap_or(path);
    let meta = std::fs::metadata(&key).ok();
    (key, meta)
}
