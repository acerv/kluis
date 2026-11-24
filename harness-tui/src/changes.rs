// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The files of the project the session changed, found by comparing them
//! with a snapshot taken at the start, on a thread of its own.
//!
//! No git command runs: a repository the model can write to may carry a
//! config that runs programs on the host. The files are only read, never
//! through a symbolic link, and only when they are regular files.

use std::collections::{HashMap, HashSet};
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::thread;

use harness_core::tools::{count_lines, diff_lines, Diff};
use ignore::WalkBuilder;
use tokio::sync::mpsc;

/// Most files looked at, in the snapshot and in each scan.
pub const MAX_FILES: usize = 50_000;

/// Largest file whose text the snapshot keeps.
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;

/// Most bytes of text the snapshot keeps in all.
pub const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;

/// How a file differs from the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// A file the session changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path relative to the project folder.
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// Lines added and removed, unknown when either version is not text
    /// kept in full.
    pub lines: Option<(usize, usize)>,
}

/// What the watcher found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangesUpdate {
    /// The changed files, sorted by path, and whether the project has more
    /// than [`MAX_FILES`] files, the ones after it not being looked at.
    Files {
        files: Vec<FileChange>,
        truncated: bool,
    },
    /// The diff of `path`, or why there is none.
    Diff {
        path: PathBuf,
        diff: Result<Diff, &'static str>,
    },
}

/// What a file holds, as far as it can be diffed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Text(Box<str>),
    /// Not diffed, for this reason.
    Opaque(&'static str),
}

/// What tells that a file changed: its size, and when its inode changed,
/// which no program can set back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    ctime: (i64, i64),
}

impl Stamp {
    fn of(meta: &Metadata) -> Self {
        Self {
            len: meta.len(),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

struct Entry {
    stamp: Stamp,
    content: Content,
}

/// The files of a folder at a point in time.
pub struct Snapshot {
    root: PathBuf,
    files: HashMap<PathBuf, Entry>,
}

impl Snapshot {
    /// Read the files of `root`, as `git` would list them, but keeping the
    /// hidden ones and the ones not committed: what `.gitignore` leaves
    /// out is not looked at, even outside a git repository.
    pub fn take(root: &Path) -> Self {
        let mut files = HashMap::new();
        let mut budget = MAX_SNAPSHOT_BYTES;
        walk(root, |path, stamp| {
            let entry = if stamp.len > MAX_TEXT_BYTES || stamp.len > budget {
                Entry {
                    stamp,
                    content: Content::Opaque("too large to diff"),
                }
            } else {
                let Some((stamp, content)) = read(&root.join(&path)) else {
                    return;
                };
                if let Content::Text(text) = &content {
                    budget -= (text.len() as u64).min(budget);
                }
                Entry { stamp, content }
            };
            files.insert(path, entry);
        });
        Self {
            root: root.to_owned(),
            files,
        }
    }

    /// The files that differ from the snapshot now.
    pub fn changes(&self) -> ChangesUpdate {
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        let truncated = walk(&self.root, |path, stamp| {
            let old = self.files.get(&path);
            if old.is_some() {
                seen.insert(path.clone());
            }
            if old.is_some_and(|old| old.stamp == stamp) {
                return;
            }
            let Some((_, new)) = read(&self.root.join(&path)) else {
                return;
            };
            let (kind, lines) = match (old.map(|old| &old.content), &new) {
                (None, Content::Text(new)) => (ChangeKind::Added, Some(count_lines("", new))),
                (None, Content::Opaque(_)) => (ChangeKind::Added, None),
                (Some(Content::Text(old)), Content::Text(new)) if old == new => return,
                (Some(Content::Text(old)), Content::Text(new)) => {
                    (ChangeKind::Modified, Some(count_lines(old, new)))
                }
                (Some(_), _) => (ChangeKind::Modified, None),
            };
            files.push(FileChange { path, kind, lines });
        });
        // Past the limit, a file not seen may still be there.
        if !truncated {
            for (path, entry) in &self.files {
                if !seen.contains(path) {
                    let lines = match &entry.content {
                        Content::Text(old) => Some(count_lines(old, "")),
                        Content::Opaque(_) => None,
                    };
                    files.push(FileChange {
                        path: path.clone(),
                        kind: ChangeKind::Deleted,
                        lines,
                    });
                }
            }
        }
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        ChangesUpdate::Files { files, truncated }
    }

    /// The diff of `path`, relative to the folder, from the snapshot to now.
    pub fn diff(&self, path: &Path) -> Result<Diff, &'static str> {
        let old = match self.files.get(path).map(|entry| &entry.content) {
            Some(Content::Text(text)) => text,
            Some(Content::Opaque(reason)) => return Err(reason),
            None => "",
        };
        let new = match read(&self.root.join(path)) {
            Some((_, Content::Text(text))) => text,
            Some((_, Content::Opaque(reason))) => return Err(reason),
            None => Box::from(""),
        };
        Ok(diff_lines(old, &new))
    }
}

/// Call `f` with the path, relative to `root`, and the stamp of each
/// regular file of `root`, up to [`MAX_FILES`]: returns whether there were
/// more.
fn walk(root: &Path, mut f: impl FnMut(PathBuf, Stamp)) -> bool {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    let mut count = 0;
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if count == MAX_FILES {
            return true;
        }
        count += 1;
        let (Ok(path), Ok(meta)) = (entry.path().strip_prefix(root), entry.metadata()) else {
            continue;
        };
        f(path.to_owned(), Stamp::of(&meta));
    }
    false
}

/// The stamp and content of the regular file at `path`, `None` when it is
/// gone or is not one. Never follows a symbolic link, nor blocks on a FIFO
/// put in place of the file.
fn read(path: &Path) -> Option<(Stamp, Content)> {
    let mut file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let stamp = Stamp::of(&meta);
    if meta.len() > MAX_TEXT_BYTES {
        return Some((stamp, Content::Opaque("too large to diff")));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    let content = match file
        .by_ref()
        .take(MAX_TEXT_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        Err(_) => Content::Opaque("cannot be read"),
        Ok(_) if bytes.len() as u64 > MAX_TEXT_BYTES => Content::Opaque("too large to diff"),
        Ok(_) if bytes.contains(&0) => Content::Opaque("binary file"),
        Ok(_) => match String::from_utf8(bytes) {
            Ok(text) => Content::Text(text.into_boxed_str()),
            Err(_) => Content::Opaque("binary file"),
        },
    };
    Some((stamp, content))
}

enum Request {
    Scan,
    Diff(PathBuf),
}

/// Takes a [`Snapshot`] of a folder on a thread of its own, then answers
/// the requests to compare the folder with it. The thread ends once the
/// watcher is dropped.
pub struct Watcher {
    requests: std_mpsc::Sender<Request>,
    updates: mpsc::UnboundedReceiver<ChangesUpdate>,
}

impl Watcher {
    pub fn spawn(root: PathBuf) -> io::Result<Self> {
        let (requests, pending) = std_mpsc::channel();
        let (tx, updates) = mpsc::unbounded_channel();
        thread::Builder::new()
            .name("kl-changes".to_owned())
            .spawn(move || serve(&root, &pending, &tx))?;
        Ok(Self { requests, updates })
    }

    /// Ask for the changed files.
    pub fn scan(&self) {
        let _ = self.requests.send(Request::Scan);
    }

    /// Ask for the diff of `path`.
    pub fn diff(&self, path: PathBuf) {
        let _ = self.requests.send(Request::Diff(path));
    }

    /// The next answer, `None` once the thread is gone.
    pub async fn next(&mut self) -> Option<ChangesUpdate> {
        self.updates.recv().await
    }
}

/// Answer `pending` requests until the watcher is dropped, the requests
/// queued meanwhile at once: a scan, then the diff asked for last.
fn serve(
    root: &Path,
    pending: &std_mpsc::Receiver<Request>,
    tx: &mpsc::UnboundedSender<ChangesUpdate>,
) {
    let snapshot = Snapshot::take(root);
    while let Ok(first) = pending.recv() {
        let mut scan = false;
        let mut diff = None;
        for request in std::iter::once(first).chain(pending.try_iter()) {
            match request {
                Request::Scan => scan = true,
                Request::Diff(path) => diff = Some(path),
            }
        }
        if scan && tx.send(snapshot.changes()).is_err() {
            return;
        }
        if let Some(path) = diff {
            let diff = snapshot.diff(&path);
            if tx.send(ChangesUpdate::Diff { path, diff }).is_err() {
                return;
            }
        }
    }
}
