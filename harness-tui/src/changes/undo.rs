// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Putting the files of the project back as they were before a prompt.
//!
//! A checkpoint holds what differed from the [`Snapshot`] when it was
//! taken: the two tell what every file was then. Files are written and
//! deleted from the host, in a folder the model can write to: no symbolic
//! link is followed on the way, so that a link left there leads nowhere
//! outside the project.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs::{File, Permissions};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use harness_core::tools::count_lines;
use rustix::fs::{openat, unlinkat, AtFlags, Dir, Mode, OFlags, CWD};
use rustix::io::Errno;

use super::{read, walk, ChangeKind, Content, Entry, FileChange, Snapshot, MAX_SNAPSHOT_BYTES};

/// Most prompts whose files can be put back.
pub const MAX_CHECKPOINTS: usize = 10;

/// Why the text of a file is not in a checkpoint.
const OVER_BUDGET: &str = "too large to keep";

/// What the files that differed from the snapshot held: `None` for one
/// that was gone.
type Delta = HashMap<PathBuf, Option<Entry>>;

/// The files as they were before a prompt.
struct Checkpoint {
    /// Messages of the conversation the prompt joined.
    messages: usize,
    /// What tells the last of them.
    tag: u64,
    delta: Delta,
}

/// What putting the files back as at a checkpoint does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoPlan {
    /// Messages the conversation had at the checkpoint.
    pub messages: usize,
    /// What tells the last of them.
    pub tag: u64,
    /// The files changed since, sorted by path, with what was done to
    /// them: an added one is deleted, a deleted one made again.
    pub files: Vec<FileChange>,
    /// The files changed since that are left as they are, and why.
    pub kept: Vec<(PathBuf, &'static str)>,
}

/// What putting the files back did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undo {
    pub plan: UndoPlan,
    /// The files of the plan that could not be put back, and why.
    pub failed: Vec<(PathBuf, String)>,
}

/// The checkpoints of the last prompts, oldest first.
#[derive(Default)]
pub(super) struct Checkpoints(VecDeque<Checkpoint>);

impl Checkpoints {
    /// Remember the files as they are now, before a prompt joins a
    /// conversation of `messages` messages. The checkpoints taken with as
    /// many messages or more are of prompts since removed from it.
    pub(super) fn push(&mut self, snapshot: &Snapshot, messages: usize, tag: u64) {
        self.0.retain(|checkpoint| checkpoint.messages < messages);
        if self.0.len() == MAX_CHECKPOINTS {
            self.0.pop_front();
        }
        let delta = snapshot.delta(self.0.back().map(|last| &last.delta));
        self.0.push_back(Checkpoint {
            messages,
            tag,
            delta,
        });
    }

    pub(super) fn plan(&self, snapshot: &Snapshot) -> Option<UndoPlan> {
        let checkpoint = self.0.back()?;
        Some(snapshot.steps(checkpoint).0)
    }

    pub(super) fn undo(&mut self, snapshot: &Snapshot) -> Option<Undo> {
        let checkpoint = self.0.pop_back()?;
        let (plan, mut steps) = snapshot.steps(&checkpoint);
        // The files to delete first: one to write may take the place of
        // the folder that held them.
        steps.sort_by_key(|(_, target)| target.is_some());
        let failed = steps
            .into_iter()
            .filter_map(|(path, target)| {
                let res = match target {
                    Some((text, mode)) => write(&snapshot.root, &path, &text, mode),
                    None => delete(&snapshot.root, &path),
                };
                Some((path, res.err()?.to_string()))
            })
            .collect();
        Some(Undo { plan, failed })
    }

    /// The messages and the tag the last checkpoint was taken with.
    pub(super) fn last(&self) -> Option<(usize, u64)> {
        let last = self.0.back()?;
        Some((last.messages, last.tag))
    }

    /// Forget the last checkpoint, or `all` of them.
    pub(super) fn forget(&mut self, all: bool) {
        match all {
            true => self.0.clear(),
            false => drop(self.0.pop_back()),
        }
    }
}

/// The text and permissions to give a file, `None` to delete it.
type Target = Option<(Arc<str>, u32)>;

impl Snapshot {
    /// What differs from the snapshot now. The text of a file `known` has
    /// with the same stamp is shared with it.
    fn delta(&self, known: Option<&Delta>) -> Delta {
        let mut delta = Delta::new();
        let mut seen = BTreeSet::new();
        let mut budget = MAX_SNAPSHOT_BYTES;
        let truncated = walk(&self.root, |path, stamp| {
            let old = self.files.get(&path);
            if old.is_some() {
                seen.insert(path.clone());
            }
            if old.is_some_and(|old| old.stamp == stamp) {
                return;
            }
            let reused = known
                .and_then(|known| known.get(&path)?.as_ref())
                .filter(|entry| entry.stamp == stamp)
                .cloned();
            let Some(mut entry) = reused.or_else(|| {
                let (stamp, content) = read(&self.root.join(&path))?;
                Some(Entry { stamp, content })
            }) else {
                return;
            };
            if let Content::Text(text) = &entry.content {
                if old.is_some_and(|old| old.content == entry.content) {
                    return;
                }
                let len = text.len() as u64;
                if len > budget {
                    entry.content = Content::Opaque(OVER_BUDGET);
                } else {
                    budget -= len;
                }
            }
            delta.insert(path, Some(entry));
        });
        // Past the limit, a file not seen may still be there.
        if !truncated {
            for path in self.files.keys().filter(|path| !seen.contains(*path)) {
                delta.insert(path.clone(), None);
            }
        }
        delta
    }

    /// What a file held with `delta`: `None` when there was none.
    fn entry<'a>(&'a self, delta: &'a Delta, path: &Path) -> Option<&'a Entry> {
        match delta.get(path) {
            Some(entry) => entry.as_ref(),
            None => self.files.get(path),
        }
    }

    /// What putting the files back as at `checkpoint` does, for the user
    /// and to do it: each file to write or delete.
    fn steps(&self, checkpoint: &Checkpoint) -> (UndoPlan, Vec<(PathBuf, Target)>) {
        let now = self.delta(Some(&checkpoint.delta));
        let paths: BTreeSet<&PathBuf> = now.keys().chain(checkpoint.delta.keys()).collect();
        let mut plan = UndoPlan {
            messages: checkpoint.messages,
            tag: checkpoint.tag,
            files: Vec::new(),
            kept: Vec::new(),
        };
        let mut steps = Vec::new();
        // The text of a file, empty for none, unless it is not known.
        let text = |entry: Option<&Entry>| match entry.map(|entry| &entry.content) {
            Some(Content::Text(text)) => Some(text.clone()),
            Some(Content::Opaque(_)) => None,
            None => Some(Arc::from("")),
        };
        for path in paths {
            let then = self.entry(&checkpoint.delta, path);
            let current = self.entry(&now, path);
            let (kind, target) = match (then, current) {
                (None, None) => continue,
                (None, Some(_)) => (ChangeKind::Added, None),
                (Some(then), None) => (ChangeKind::Deleted, Some(then)),
                (Some(then), Some(current)) => {
                    let same_text = matches!(
                        (&then.content, &current.content),
                        (Content::Text(a), Content::Text(b)) if a == b
                    );
                    if then.stamp == current.stamp || same_text {
                        continue;
                    }
                    (ChangeKind::Modified, Some(then))
                }
            };
            let target = match target.map(|then| (&then.content, then.stamp.mode)) {
                Some((Content::Opaque(reason), _)) => {
                    plan.kept.push((path.clone(), *reason));
                    continue;
                }
                Some((Content::Text(text), mode)) => Some((text.clone(), mode)),
                None => None,
            };
            let lines = text(then)
                .zip(text(current))
                .map(|(then, current)| count_lines(&then, &current));
            plan.files.push(FileChange {
                path: path.clone(),
                kind,
                lines,
            });
            steps.push((path.clone(), target));
        }
        (plan, steps)
    }
}

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// Open the folder holding `path`, relative to `root`, going through
/// folders only, never a symbolic link, and making the missing ones with
/// `create`. Returns it with the name of the file in it.
fn open_parent<'a>(root: &Path, path: &'a Path, create: bool) -> io::Result<(OwnedFd, &'a Path)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "not a path of the project");
    let mut dir = openat(CWD, root, DIR, Mode::empty())?;
    let mut names = path.components().peekable();
    while let Some(component) = names.next() {
        let Component::Normal(name) = component else {
            return Err(invalid());
        };
        if names.peek().is_none() {
            return Ok((dir, Path::new(name)));
        }
        let open = |dir: &OwnedFd| openat(dir, name, DIR | OFlags::NOFOLLOW, Mode::empty());
        dir = match open(&dir) {
            Err(Errno::NOENT) if create => {
                rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o755))?;
                open(&dir)?
            }
            res => res?,
        };
    }
    Err(invalid())
}

/// Give the file at `path`, relative to `root`, the `text` and the
/// permissions `mode`, making it and its folders when missing. What is not
/// a regular file there is replaced when it can be deleted: a folder, when
/// it holds no file.
fn write(root: &Path, path: &Path, text: &str, mode: u32) -> io::Result<()> {
    let (dir, name) = open_parent(root, path, true)?;
    let mode = mode & 0o777;
    let open = || {
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = File::from(openat(&dir, name, flags, Mode::from_raw_mode(mode))?);
        match file.metadata()?.is_file() {
            true => Ok(file),
            false => Err(io::Error::other("not a regular file")),
        }
    };
    let mut file = open().or_else(|_| {
        // A link or a FIFO left in its place, or a folder.
        unlinkat(&dir, name, AtFlags::empty()).or_else(|_| remove_folders(&dir, name))?;
        open()
    })?;
    file.set_len(0)?;
    file.write_all(text.as_bytes())?;
    file.set_permissions(Permissions::from_mode(mode))
}

/// Remove the folder `name` of `dir` and the folders in it, failing when
/// one holds anything else.
fn remove_folders(dir: &OwnedFd, name: &Path) -> io::Result<()> {
    let folder = openat(dir, name, DIR | OFlags::NOFOLLOW, Mode::empty())?;
    for entry in Dir::read_from(&folder)? {
        let entry = entry?;
        let inner = entry.file_name().to_bytes();
        if inner != b"." && inner != b".." {
            remove_folders(&folder, Path::new(OsStr::from_bytes(inner)))?;
        }
    }
    Ok(unlinkat(dir, name, AtFlags::REMOVEDIR)?)
}

/// Delete the file at `path`, relative to `root`, if it is still there.
fn delete(root: &Path, path: &Path) -> io::Result<()> {
    let (dir, name) = match open_parent(root, path, false) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        res => res?,
    };
    match unlinkat(&dir, name, AtFlags::empty()) {
        Err(Errno::NOENT) => Ok(()),
        res => Ok(res?),
    }
}
