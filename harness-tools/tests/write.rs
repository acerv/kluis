// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use common::p;
use harness_tools::WriteTool;

fn call(path: impl Into<String>, content: &str) -> WriteTool {
    WriteTool {
        path: path.into(),
        content: content.into(),
    }
}

#[test]
fn definition() {
    let tool = WriteTool::definition();
    assert_eq!(tool.name.as_ref(), "write");
    assert_eq!(tool.schema, Some(WriteTool::schema()));
}

#[tokio::test]
async fn new_and_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");

    let out = call(p(&f), "hello").run().await.unwrap();
    assert_eq!(out.bytes_written, 5);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello");

    call(p(&f), "bye").run().await.unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "bye");
}

#[tokio::test]
async fn creates_parents() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("x/y/z.txt");

    call(p(&f), "deep").run().await.unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "deep");
}

// The only test in this binary that changes $HOME.
#[tokio::test]
async fn tilde_expands_to_home() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());

    call("~/sub/t.txt", "home").run().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(home.path().join("sub/t.txt")).unwrap(),
        "home"
    );
}

#[tokio::test]
async fn counts_bytes_not_chars() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("utf8");
    let content = "héllo 🦀";

    let out = call(p(&f), content).run().await.unwrap();
    assert_eq!(out.bytes_written, content.len());
    assert_ne!(out.bytes_written, content.chars().count());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), content);
}

#[tokio::test]
async fn directory_target_fails() {
    let dir = tempfile::tempdir().unwrap();

    let err = call(p(dir.path()), "x").run().await.unwrap_err();
    assert!(err.message.contains(&p(dir.path())));
    assert!(err.message.contains("is a directory"));
}

#[tokio::test]
async fn trailing_slash_fails() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "keep").unwrap();

    // Nothing exists at the path.
    let new = format!("{}/new/", p(dir.path()));
    let err = call(new.clone(), "x").run().await.unwrap_err();
    assert!(err.message.contains(&new));
    assert!(err.message.contains("ends with '/'"));
    assert!(!dir.path().join("new").exists());

    // An existing regular file.
    let err = call(format!("{}/", p(&file)), "x").run().await.unwrap_err();
    assert!(err.message.contains("ends with '/'"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");

    // An existing directory.
    let err = call(format!("{}/", p(dir.path())), "x")
        .run()
        .await
        .unwrap_err();
    assert!(err.message.contains("ends with '/'"));
}

#[tokio::test]
async fn special_files_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success());

    for path in [fifo.as_path(), std::path::Path::new("/dev/null")] {
        let fut = call(p(path), "x").run();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .unwrap_or_else(|_| panic!("{} hung", path.display()));
        let err = r.unwrap_err();
        assert!(err.message.contains("not a regular file"), "{}", err);
    }
}

#[tokio::test]
async fn broken_symlink_fails() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("missing.txt");
    let link = dir.path().join("dangling");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let err = call(p(&link), "x").run().await.unwrap_err();
    assert!(err.message.contains("broken symlink"));
    assert!(!target.exists());
}
