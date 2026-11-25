// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! `edit` and `write` refuse a file that changed since the model saw it.

use std::path::Path;

use harness_exec::{dispatch_raw, ToolContext};
use serde_json::{json, Value};

const STALE: &str = "changed since you last read it";

fn context(dir: &Path) -> ToolContext {
    ToolContext {
        cwd: dir.to_path_buf(),
        ..ToolContext::default()
    }
}

async fn run(ctx: &ToolContext, name: &str, args: Value) -> String {
    dispatch_raw(name, &args, ctx, |_| {}).await.0
}

async fn read(ctx: &ToolContext, path: &str) -> String {
    run(ctx, "read", json!({ "path": path })).await
}

async fn edit(ctx: &ToolContext, path: &str, old: &str, new: &str) -> String {
    let edits = json!([{ "oldText": old, "newText": new }]);
    run(ctx, "edit", json!({ "path": path, "edits": edits })).await
}

async fn write(ctx: &ToolContext, path: &str, content: &str) -> String {
    run(ctx, "write", json!({ "path": path, "content": content })).await
}

/// Change `path` behind the model's back. The size changes too, so the
/// test does not depend on the clock of the filesystem.
fn change(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn edit_refuses_a_file_changed_after_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let ctx = context(dir.path());

    read(&ctx, "a.txt").await;
    change(&file, "one\nmine\n");
    let out = edit(&ctx, "a.txt", "one", "two").await;
    assert!(out.contains(STALE), "{out}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\nmine\n");

    // Reading it again is enough.
    read(&ctx, "a.txt").await;
    let out = edit(&ctx, "a.txt", "one", "two").await;
    assert!(out.contains("\"replacements\":1"), "{out}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "two\nmine\n");
}

#[tokio::test]
async fn write_refuses_a_file_changed_after_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let ctx = context(dir.path());

    read(&ctx, "a.txt").await;
    change(&file, "one\nmine\n");
    let out = write(&ctx, "a.txt", "all new\n").await;
    assert!(out.contains(STALE), "{out}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\nmine\n");
}

#[tokio::test]
async fn files_never_read_are_changed_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let ctx = context(dir.path());

    let out = edit(&ctx, "a.txt", "one", "two").await;
    assert!(out.contains("\"replacements\":1"), "{out}");
    let out = write(&ctx, "new.txt", "hello\n").await;
    assert!(out.contains("bytes_written"), "{out}");
}

#[tokio::test]
async fn own_changes_count_as_seen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    let ctx = context(dir.path());

    // A write, then edits in a row, need no read in between.
    write(&ctx, "a.txt", "one\n").await;
    let out = edit(&ctx, "a.txt", "one", "two").await;
    assert!(out.contains("\"replacements\":1"), "{out}");
    let out = edit(&ctx, "a.txt", "two", "three").await;
    assert!(out.contains("\"replacements\":1"), "{out}");

    // But a change after them is noticed.
    change(&file, "three\nmine\n");
    let out = edit(&ctx, "a.txt", "three", "four").await;
    assert!(out.contains(STALE), "{out}");
}

#[tokio::test]
async fn a_file_deleted_after_the_read_can_be_written_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let ctx = context(dir.path());

    // Nothing is left to overwrite, and reading it again would fail.
    read(&ctx, "a.txt").await;
    std::fs::remove_file(&file).unwrap();
    let out = write(&ctx, "a.txt", "back\n").await;
    assert!(out.contains("bytes_written"), "{out}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "back\n");
}

#[tokio::test]
async fn other_names_of_a_file_share_what_was_seen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\n").unwrap();
    std::os::unix::fs::symlink(&file, dir.path().join("link.txt")).unwrap();
    let ctx = context(dir.path());

    read(&ctx, "link.txt").await;
    change(&file, "one\nmine\n");
    let out = edit(&ctx, "./a.txt", "one", "two").await;
    assert!(out.contains(STALE), "{out}");
}

#[tokio::test]
async fn clones_of_a_context_share_what_was_seen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let ctx = context(dir.path());

    read(&ctx.clone(), "a.txt").await;
    change(&file, "one\nmine\n");
    let out = edit(&ctx, "a.txt", "one", "two").await;
    assert!(out.contains(STALE), "{out}");
}
