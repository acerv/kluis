// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use harness_core::tools::DiffLine;
use harness_tui::{
    ChangeKind, ChangesUpdate, FileChange, Snapshot, Watcher, MAX_CHECKPOINTS, MAX_TEXT_BYTES,
};

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

/// A watcher of `root` once it took its snapshot.
async fn watching(root: &Path) -> Watcher {
    let mut watcher = Watcher::spawn(root.to_owned()).unwrap();
    watcher.scan();
    next(&mut watcher).await;
    watcher
}

fn text(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn undo_puts_the_files_back_as_at_the_checkpoint() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(&root.join("src/main.rs"), "one\ntwo\n");
    put(&root.join("run.sh"), "#!/bin/sh\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    put(&root.join("docs/old/notes.txt"), "notes\n");
    let watcher = watching(root).await;
    assert_eq!(watcher.undo_plan().await, None);
    assert_eq!(watcher.undo().await, None);

    // Changed before the prompt: this is what comes back, not the start.
    put(&root.join("src/main.rs"), "one\ntwo\nthree\n");
    watcher.checkpoint(4, 7).await;

    put(&root.join("src/main.rs"), "1\ntwo\nthree\n");
    fs::remove_file(root.join("run.sh")).unwrap();
    fs::remove_dir_all(root.join("docs")).unwrap();
    put(&root.join("new/deep/file.txt"), "n\n");

    let plan = watcher.undo_plan().await.unwrap();
    assert_eq!((plan.messages, plan.tag), (4, 7));
    assert_eq!(
        plan.files,
        vec![
            change("docs/old/notes.txt", ChangeKind::Deleted, Some((0, 1))),
            change("new/deep/file.txt", ChangeKind::Added, Some((1, 0))),
            change("run.sh", ChangeKind::Deleted, Some((0, 1))),
            change("src/main.rs", ChangeKind::Modified, Some((1, 1))),
        ]
    );
    assert!(plan.kept.is_empty());
    // Looking changes nothing.
    assert_eq!(text(&root.join("src/main.rs")), "1\ntwo\nthree\n");

    let undo = watcher.undo().await.unwrap();
    assert_eq!(undo.plan, plan);
    assert!(undo.failed.is_empty(), "{:?}", undo.failed);
    assert_eq!(text(&root.join("src/main.rs")), "one\ntwo\nthree\n");
    assert_eq!(text(&root.join("docs/old/notes.txt")), "notes\n");
    assert!(!root.join("new/deep/file.txt").exists());
    let mode = fs::metadata(root.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);

    // The checkpoint is used up.
    assert_eq!(watcher.undo_plan().await, None);
}

#[tokio::test]
async fn undo_walks_back_one_prompt_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    put(&file, "start\n");
    let watcher = watching(dir.path()).await;

    watcher.checkpoint(0, 0).await;
    put(&file, "first\n");
    watcher.checkpoint(2, 0).await;
    put(&file, "second\n");

    assert!(watcher.undo().await.unwrap().failed.is_empty());
    assert_eq!(text(&file), "first\n");
    assert!(watcher.undo().await.unwrap().failed.is_empty());
    assert_eq!(text(&file), "start\n");
    assert_eq!(watcher.undo().await, None);
}

#[tokio::test]
async fn checkpoints_of_prompts_sent_again_are_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    put(&file, "start\n");
    let watcher = watching(dir.path()).await;

    watcher.checkpoint(0, 0).await;
    put(&file, "first\n");
    watcher.checkpoint(2, 0).await;
    put(&file, "second\n");
    // The second prompt is sent again, as `/retry` does.
    watcher.checkpoint(2, 0).await;
    put(&file, "second again\n");

    watcher.undo().await.unwrap();
    assert_eq!(text(&file), "second\n");
    watcher.undo().await.unwrap();
    assert_eq!(text(&file), "start\n");
    assert_eq!(watcher.undo().await, None);

    // Forgetting one leaves the files alone.
    watcher.checkpoint(0, 0).await;
    put(&file, "kept\n");
    assert_eq!(watcher.last_checkpoint().await, Some((0, 0)));
    watcher.forget_checkpoints(false);
    assert_eq!(watcher.last_checkpoint().await, None);
    assert_eq!(watcher.undo().await, None);
    assert_eq!(text(&file), "kept\n");

    // All of them, when the conversation is another one.
    watcher.checkpoint(0, 0).await;
    watcher.checkpoint(2, 0).await;
    watcher.forget_checkpoints(true);
    assert_eq!(watcher.undo().await, None);
}

#[tokio::test]
async fn only_the_last_checkpoints_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    put(&file, "0\n");
    let watcher = watching(dir.path()).await;
    for prompt in 1..=MAX_CHECKPOINTS + 2 {
        watcher.checkpoint(prompt, 0).await;
        put(&file, &format!("{prompt}\n"));
    }
    let mut undone = 0;
    while watcher.undo().await.is_some() {
        undone += 1;
    }
    assert_eq!(undone, MAX_CHECKPOINTS);
    assert_eq!(text(&file), "2\n");
}

#[tokio::test]
async fn undo_leaves_what_it_cannot_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("image.bin"), [0u8, 1, 2]).unwrap();
    put(&root.join("a.txt"), "a\n");
    let watcher = watching(root).await;
    watcher.checkpoint(0, 0).await;

    fs::write(root.join("image.bin"), [3u8, 0, 4, 5]).unwrap();
    // A binary file made since can be deleted, though.
    fs::write(root.join("new.bin"), [0u8]).unwrap();
    put(&root.join("a.txt"), "b\n");

    let undo = watcher.undo().await.unwrap();
    assert_eq!(
        undo.plan.kept,
        vec![(PathBuf::from("image.bin"), "binary file")]
    );
    assert_eq!(
        undo.plan.files,
        vec![
            change("a.txt", ChangeKind::Modified, Some((1, 1))),
            change("new.bin", ChangeKind::Added, None),
        ]
    );
    assert!(undo.failed.is_empty(), "{:?}", undo.failed);
    assert_eq!(fs::read(root.join("image.bin")).unwrap(), [3u8, 0, 4, 5]);
    assert!(!root.join("new.bin").exists());
    assert_eq!(text(&root.join("a.txt")), "a\n");
}

#[tokio::test]
async fn undo_follows_no_link_out_of_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    put(&root.join("sub/a.txt"), "inside\n");
    put(&root.join("b.txt"), "b\n");
    put(&root.join("gone/c.txt"), "c\n");
    put(&outside.join("a.txt"), "outside\n");
    put(&outside.join("secret"), "secret\n");
    put(&outside.join("c.txt"), "outside c\n");
    let watcher = watching(&root).await;
    watcher.checkpoint(0, 0).await;

    // As a command of the model could leave them: a folder and a file
    // replaced by links to the outside, a file deleted through a link.
    fs::remove_dir_all(root.join("sub")).unwrap();
    symlink(&outside, root.join("sub")).unwrap();
    fs::remove_file(root.join("b.txt")).unwrap();
    symlink(outside.join("secret"), root.join("b.txt")).unwrap();

    let undo = watcher.undo().await.unwrap();
    // The folder is not gone through: its file cannot be made again.
    let failed: Vec<_> = undo.failed.iter().map(|(path, _)| path.clone()).collect();
    assert_eq!(failed, vec![PathBuf::from("sub/a.txt")]);
    assert_eq!(text(&outside.join("a.txt")), "outside\n");
    // The link is replaced by the file, what it names is left alone.
    assert_eq!(text(&outside.join("secret")), "secret\n");
    assert!(!root.join("b.txt").is_symlink());
    assert_eq!(text(&root.join("b.txt")), "b\n");

    // A file added since is not deleted through a link either.
    watcher.checkpoint(0, 0).await;
    put(&root.join("gone/new.txt"), "new\n");
    let moved = dir.path().join("moved");
    fs::rename(root.join("gone"), &moved).unwrap();
    put(&outside.join("new.txt"), "outside new\n");
    symlink(&outside, root.join("gone")).unwrap();
    watcher.undo().await.unwrap();
    assert_eq!(text(&outside.join("new.txt")), "outside new\n");
    assert_eq!(text(&outside.join("c.txt")), "outside c\n");
}

#[tokio::test]
async fn undo_keeps_a_file_that_could_not_be_read() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file = root.join("notes.txt");
    put(&file, "notes\n");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&file).is_ok() {
        // Root reads any file.
        return;
    }
    let watcher = watching(root).await;
    watcher.checkpoint(0, 0).await;
    // Readable since the prompt: not a file it added.
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

    let plan = watcher.undo_plan().await.unwrap();
    assert!(plan.files.is_empty(), "{:?}", plan.files);
    assert_eq!(plan.kept, [(PathBuf::from("notes.txt"), "cannot be read")]);
    watcher.undo().await.unwrap();
    assert_eq!(text(&file), "notes\n");
}

#[tokio::test]
async fn undo_puts_back_a_file_replaced_by_a_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    put(&root.join("a"), "text\n");
    let watcher = watching(root).await;
    watcher.checkpoint(0, 0).await;
    fs::remove_file(root.join("a")).unwrap();
    put(&root.join("a/deep/inner.txt"), "x\n");
    fs::create_dir(root.join("a/empty")).unwrap();

    let undo = watcher.undo().await.unwrap();
    assert!(undo.failed.is_empty(), "{:?}", undo.failed);
    assert_eq!(text(&root.join("a")), "text\n");
}
