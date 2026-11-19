// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use common::p;
use harness_exec::WriteTool;

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
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    let reference = dir.path().join("ref.txt");
    std::fs::write(&reference, "ref").unwrap();

    let out = call(p(&f), "hello").run().await.unwrap();
    assert_eq!(out.bytes_written, 5);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello");
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        std::fs::metadata(&reference).unwrap().permissions().mode() & 0o777,
    );

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

#[tokio::test]
async fn relative_path_starts_at_cwd() {
    let dir = tempfile::tempdir().unwrap();

    call("sub/a.txt", "inside")
        .run_in(dir.path())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sub/a.txt")).unwrap(),
        "inside"
    );
}

#[tokio::test]
async fn atomic_overwrite_preserves_mode_and_replaces_inode() {
    use std::fs::Permissions;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("script.sh");
    std::fs::write(&f, "old content").unwrap();
    std::fs::set_permissions(&f, Permissions::from_mode(0o750)).unwrap();

    let mut old_reader = std::fs::File::open(&f).unwrap();
    let old_ino = std::fs::metadata(&f).unwrap().ino();

    call(p(&f), "new content").run().await.unwrap();

    let meta = std::fs::metadata(&f).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o750);
    assert_ne!(meta.ino(), old_ino);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "new content");

    // Existing open descriptor still sees the old inode untouched.
    let mut old = String::new();
    old_reader.read_to_string(&mut old).unwrap();
    assert_eq!(old, "old content");

    // No temp files left behind.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn follows_symlink_and_keeps_link_intact() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real.txt");
    let link = dir.path().join("link.txt");
    std::fs::write(&target, "before").unwrap();
    std::fs::set_permissions(&target, Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    call(p(&link), "after").run().await.unwrap();

    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
        0o600
    );
}

#[tokio::test]
async fn read_only_file_is_not_overwritten_and_read_only_dir_falls_back() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let ro = dir.path().join("ro.txt");
    std::fs::write(&ro, "keep").unwrap();
    std::fs::set_permissions(&ro, Permissions::from_mode(0o444)).unwrap();

    let err = call(p(&ro), "overwrite").run().await.unwrap_err();
    assert!(err.message.contains("Permission denied"), "{err}");
    assert_eq!(std::fs::read_to_string(&ro).unwrap(), "keep");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    // Writable file inside a read-only directory falls back to writing in place.
    let ro_dir = dir.path().join("ro_dir");
    std::fs::create_dir(&ro_dir).unwrap();
    let f = ro_dir.join("writable.txt");
    std::fs::write(&f, "before").unwrap();
    std::fs::set_permissions(&ro_dir, Permissions::from_mode(0o555)).unwrap();

    call(p(&f), "after").run().await.unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "after");

    std::fs::set_permissions(&ro_dir, Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&ro, Permissions::from_mode(0o644)).unwrap();
}
