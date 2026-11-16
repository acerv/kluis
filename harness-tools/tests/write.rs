mod common;

use common::p;
use harness_tools::{write, ToolCall, ToolOutput, WriteOutput, WriteParams};
use serde_json::json;

#[test]
fn call_roundtrip() {
    let call = ToolCall::Write(WriteParams {
        path: "a.txt".into(),
        content: "hi".into(),
    });
    let v = serde_json::to_value(&call).unwrap();
    assert_eq!(
        v,
        json!({"name":"write","input":{"path":"a.txt","content":"hi"}})
    );
    assert_eq!(serde_json::from_value::<ToolCall>(v).unwrap(), call);
}

#[test]
fn name_case_variants() {
    for name in ["write", "Write", "WRITE"] {
        let call: ToolCall =
            serde_json::from_value(json!({"name":name,"input":{"path":"a","content":"x"}}))
                .unwrap();
        assert!(matches!(call, ToolCall::Write(_)), "{name}");
        assert_eq!(serde_json::to_value(&call).unwrap()["name"], "write");
    }

    let r = serde_json::from_value::<ToolCall>(
        json!({"name":"wRiTe","input":{"path":"a","content":"x"}}),
    );
    assert!(r.is_err());
}

#[test]
fn missing_content_fails() {
    let r = serde_json::from_value::<ToolCall>(json!({"name":"write","input":{"path":"a"}}));
    assert!(r.is_err());
}

#[test]
fn wrong_types_fail() {
    for input in [
        json!({"path":"a","content":1}),
        json!({"path":null,"content":"x"}),
    ] {
        let r = serde_json::from_value::<ToolCall>(json!({"name":"write","input":input}));
        assert!(r.is_err(), "accepted {input}");
    }
}

#[test]
fn unknown_fields_ignored() {
    let call: ToolCall = serde_json::from_value(
        json!({"name":"write","input":{"path":"a","content":"x","mode":"append"}}),
    )
    .unwrap();
    assert_eq!(
        call,
        ToolCall::Write(WriteParams {
            path: "a".into(),
            content: "x".into(),
        })
    );
}

#[test]
fn output_tagged_shape() {
    let out = ToolOutput::Write(WriteOutput { bytes_written: 3 });
    let v = serde_json::to_value(&out).unwrap();
    assert_eq!(v, json!({"name":"write","output":{"bytes_written":3}}));
    assert_eq!(serde_json::from_value::<ToolOutput>(v).unwrap(), out);
}

#[tokio::test]
async fn new_and_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");

    let out = write(WriteParams {
        path: p(&f),
        content: "hello".into(),
    })
    .await
    .unwrap();
    assert_eq!(out.bytes_written, 5);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello");

    write(WriteParams {
        path: p(&f),
        content: "bye".into(),
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "bye");
}

#[tokio::test]
async fn creates_parents() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("x/y/z.txt");

    write(WriteParams {
        path: p(&f),
        content: "deep".into(),
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "deep");
}

// The only test in this binary that changes $HOME.
#[tokio::test]
async fn tilde_expands_to_home() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());

    write(WriteParams {
        path: "~/sub/t.txt".into(),
        content: "home".into(),
    })
    .await
    .unwrap();
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

    write(WriteParams {
        path: "sub/rel.txt".into(),
        content: "relative\n".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sub/rel.txt")).unwrap(),
        "relative\n"
    );
}

#[tokio::test]
async fn empty_content() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("empty");

    let out = write(WriteParams {
        path: p(&f),
        content: String::new(),
    })
    .await
    .unwrap();
    assert_eq!(out.bytes_written, 0);
    assert_eq!(std::fs::metadata(&f).unwrap().len(), 0);
}

#[tokio::test]
async fn counts_bytes_not_chars() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("utf8");
    let content = "héllo 🦀";

    let out = write(WriteParams {
        path: p(&f),
        content: content.into(),
    })
    .await
    .unwrap();
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
    let err = write(WriteParams {
        path: p(dir.path()),
        content: "x".into(),
    })
    .await
    .unwrap_err();
    assert!(err.message.contains(&p(dir.path())));
    assert!(err.message.contains("is a directory"));

    // A parent component is a regular file.
    let nested = file.join("child.txt");
    let err = write(WriteParams {
        path: p(&nested),
        content: "x".into(),
    })
    .await
    .unwrap_err();
    assert!(err.message.contains("child.txt"));

    // Empty path.
    let r = write(WriteParams {
        path: String::new(),
        content: "x".into(),
    })
    .await;
    assert!(r.is_err());
}

#[tokio::test]
async fn trailing_slash_fails() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "keep").unwrap();

    // Nothing exists at the path.
    let new = format!("{}/new/", p(dir.path()));
    let err = write(WriteParams {
        path: new.clone(),
        content: "x".into(),
    })
    .await
    .unwrap_err();
    assert!(err.message.contains(&new));
    assert!(err.message.contains("ends with '/'"));
    assert!(!dir.path().join("new").exists());

    // An existing regular file.
    let err = write(WriteParams {
        path: format!("{}/", p(&file)),
        content: "x".into(),
    })
    .await
    .unwrap_err();
    assert!(err.message.contains("ends with '/'"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");

    // An existing directory.
    let err = write(WriteParams {
        path: format!("{}/", p(dir.path())),
        content: "x".into(),
    })
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

    write(WriteParams {
        path: p(&link),
        content: "new".into(),
    })
    .await
    .unwrap();
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
    let r = write(WriteParams {
        path: p(&f),
        content: "x".into(),
    })
    .await;

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
        let fut = write(WriteParams {
            path: p(path),
            content: "x".into(),
        });
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

    write(WriteParams {
        path: p(&f),
        content: "#!/bin/sh\necho hi\n".into(),
    })
    .await
    .unwrap();

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

    let r = write(WriteParams {
        path: p(&f),
        content: "changed".into(),
    })
    .await;

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

    let err = write(WriteParams {
        path: p(&link),
        content: "x".into(),
    })
    .await
    .unwrap_err();
    assert!(err.message.contains("broken symlink"));
    assert!(!target.exists());
}
