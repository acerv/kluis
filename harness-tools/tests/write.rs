mod common;

use common::p;
use harness_tools::{WriteOutput, WriteTool};
use serde_json::json;

fn call(path: impl Into<String>, content: &str) -> WriteTool {
    WriteTool {
        path: path.into(),
        content: content.into(),
    }
}

fn parse(v: serde_json::Value) -> Result<WriteTool, serde_json::Error> {
    serde_json::from_value(v)
}

#[test]
fn input_roundtrip() {
    let tool = call("a.txt", "hi");
    let v = serde_json::to_value(&tool).unwrap();
    assert_eq!(v, json!({"path":"a.txt","content":"hi"}));
    assert_eq!(parse(v).unwrap(), tool);
}

#[test]
fn missing_fields_fail() {
    assert!(parse(json!({"path":"a"})).is_err());
    assert!(parse(json!({"content":"x"})).is_err());
}

#[test]
fn wrong_types_fail() {
    for input in [
        json!({"path":"a","content":1}),
        json!({"path":null,"content":"x"}),
    ] {
        assert!(parse(input.clone()).is_err(), "accepted {input}");
    }
}

#[test]
fn unknown_fields_ignored() {
    assert_eq!(
        parse(json!({"path":"a","content":"x","mode":"append"})).unwrap(),
        call("a", "x")
    );
}

#[test]
fn output_shape() {
    let out = WriteOutput { bytes_written: 3 };
    let v = serde_json::to_value(&out).unwrap();
    assert_eq!(v, json!({"bytes_written":3}));
    assert_eq!(serde_json::from_value::<WriteOutput>(v).unwrap(), out);
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

// The only test in this binary that changes the working directory;
// all other tests use absolute paths.
#[tokio::test]
async fn relative_path_uses_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    call("sub/rel.txt", "relative\n").run().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sub/rel.txt")).unwrap(),
        "relative\n"
    );
}

#[tokio::test]
async fn empty_content() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("empty");

    let out = call(p(&f), "").run().await.unwrap();
    assert_eq!(out.bytes_written, 0);
    assert_eq!(std::fs::metadata(&f).unwrap().len(), 0);
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
async fn invalid_targets_fail() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();

    // Target is a directory.
    let err = call(p(dir.path()), "x").run().await.unwrap_err();
    assert!(err.message.contains(&p(dir.path())));
    assert!(err.message.contains("is a directory"));

    // A parent component is a regular file.
    let nested = file.join("child.txt");
    let err = call(p(&nested), "x").run().await.unwrap_err();
    assert!(err.message.contains("child.txt"));

    // Empty path.
    assert!(call("", "x").run().await.is_err());
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

#[cfg(unix)]
#[tokio::test]
async fn symlink_writes_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    std::fs::write(&target, "old").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    call(p(&link), "new").run().await.unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[tokio::test]
async fn permission_denied() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let ro = dir.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();

    let f = ro.join("a.txt");
    let r = call(p(&f), "x").run().await;

    // Restore so tempdir cleanup works.
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Root bypasses permission checks; nothing to verify then.
    if r.is_ok() {
        return;
    }
    assert!(r.unwrap_err().message.contains("a.txt"));
}

#[cfg(unix)]
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

#[cfg(unix)]
#[tokio::test]
async fn overwrite_keeps_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("script.sh");
    std::fs::write(&f, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();

    call(p(&f), "#!/bin/sh\necho hi\n").run().await.unwrap();

    let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "#!/bin/sh\necho hi\n");
}

#[cfg(unix)]
#[tokio::test]
async fn read_only_file_fails() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("ro.txt");
    std::fs::write(&f, "keep").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();

    let r = call(p(&f), "changed").run().await;

    // Root bypasses permission checks; nothing to verify then.
    if r.is_ok() {
        return;
    }
    assert!(r.unwrap_err().message.contains("ro.txt"));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "keep");
}

#[cfg(unix)]
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
