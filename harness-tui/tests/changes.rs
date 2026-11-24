// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use harness_core::tools::DiffLine;
use harness_tui::{ChangeKind, ChangesUpdate, FileChange, Snapshot, Watcher, MAX_TEXT_BYTES};

fn change(path: &str, kind: ChangeKind, lines: Option<(usize, usize)>) -> FileChange {
    FileChange {
        path: PathBuf::from(path),
        kind,
        lines,
    }
}

fn files(update: ChangesUpdate) -> Vec<FileChange> {
    match update {
        ChangesUpdate::Files { files, truncated } => {
            assert!(!truncated);
            files
        }
        other => panic!("not files: {other:?}"),
    }
}

/// Write `text` to `path`, creating its folders.
fn put(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn nothing_changed() {
    let dir = tempfile::tempdir().unwrap();
    put(&dir.path().join("a.txt"), "a\n");
    let snapshot = Snapshot::take(dir.path());
    // Touched, with the same text.
    put(&dir.path().join("a.txt"), "a\n");
    assert!(files(snapshot.changes()).is_empty());
}

#[test]
fn added_modified_and_deleted_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(&root.join("src/main.rs"), "one\ntwo\nthree\n");
    put(&root.join("old.txt"), "x\ny\n");
    put(&root.join(".hidden"), "h\n");
    let snapshot = Snapshot::take(root);

    put(&root.join("src/main.rs"), "one\n2\nthree\nfour\n");
    fs::remove_file(root.join("old.txt")).unwrap();
    put(&root.join("new/file.txt"), "n\n");
    put(&root.join(".hidden"), "H\n");

    assert_eq!(
        files(snapshot.changes()),
        vec![
            change(".hidden", ChangeKind::Modified, Some((1, 1))),
            change("new/file.txt", ChangeKind::Added, Some((1, 0))),
            change("old.txt", ChangeKind::Deleted, Some((0, 2))),
            change("src/main.rs", ChangeKind::Modified, Some((2, 1))),
        ]
    );
}

#[test]
fn ignored_and_git_files_are_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(&root.join(".gitignore"), "target/\n");
    let snapshot = Snapshot::take(root);
    put(&root.join("target/out.o"), "o\n");
    put(&root.join(".git/config"), "[core]\n");
    assert!(files(snapshot.changes()).is_empty());
}

#[test]
fn binary_large_and_special_files_have_no_line_counts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(&root.join("bin"), "a\0b");
    let snapshot = Snapshot::take(root);
    put(&root.join("bin"), "a\0c");
    let large = "x".repeat(MAX_TEXT_BYTES as usize + 1);
    put(&root.join("large.txt"), &large);
    // Never followed nor read.
    symlink("/etc/passwd", root.join("link")).unwrap();
    let fifo = std::ffi::CString::new(root.join("fifo").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    assert_eq!(
        files(snapshot.changes()),
        vec![
            change("bin", ChangeKind::Modified, None),
            change("large.txt", ChangeKind::Added, None),
        ]
    );
    assert_eq!(snapshot.diff(Path::new("bin")), Err("binary file"));
    assert_eq!(
        snapshot.diff(Path::new("large.txt")),
        Err("too large to diff")
    );
}

#[test]
fn diff_from_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(&root.join("a.txt"), "a\nb\n");
    let snapshot = Snapshot::take(root);
    put(&root.join("a.txt"), "a\nB\n");
    put(&root.join("new.txt"), "n\n");

    let lines = |path: &str| -> Vec<DiffLine> {
        let diff = snapshot.diff(Path::new(path)).unwrap();
        diff.hunks.into_iter().flat_map(|h| h.lines).collect()
    };
    assert_eq!(
        lines("a.txt"),
        vec![
            DiffLine::Context("a".into()),
            DiffLine::Removed("b".into()),
            DiffLine::Added("B".into()),
        ]
    );
    assert_eq!(lines("new.txt"), vec![DiffLine::Added("n".into())]);
    fs::remove_file(root.join("a.txt")).unwrap();
    assert_eq!(
        lines("a.txt"),
        vec![DiffLine::Removed("a".into()), DiffLine::Removed("b".into())]
    );
}

async fn next(watcher: &mut Watcher) -> ChangesUpdate {
    let next = tokio::time::timeout(Duration::from_secs(10), watcher.next());
    next.await.unwrap().unwrap()
}

#[tokio::test]
async fn watcher_answers_scans_and_diffs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(&root.join("a.txt"), "a\n");
    let mut watcher = Watcher::spawn(root.to_owned()).unwrap();
    watcher.scan();
    assert!(files(next(&mut watcher).await).is_empty());

    put(&root.join("a.txt"), "b\n");
    watcher.scan();
    watcher.diff(PathBuf::from("a.txt"));
    assert_eq!(
        files(next(&mut watcher).await),
        vec![change("a.txt", ChangeKind::Modified, Some((1, 1)))]
    );
    match next(&mut watcher).await {
        ChangesUpdate::Diff { path, diff } => {
            assert_eq!(path, PathBuf::from("a.txt"));
            assert_eq!(diff.unwrap().hunks.len(), 1);
        }
        other => panic!("not a diff: {other:?}"),
    }
}
