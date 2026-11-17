// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;
use std::time::Duration;

use common::p;
use harness_tools::{ReadTool, DEFAULT_READ_LIMIT};

fn call(path: &Path, offset: Option<usize>, limit: Option<usize>) -> ReadTool {
    ReadTool {
        path: p(path),
        offset,
        limit,
    }
}

#[test]
fn definition() {
    let tool = ReadTool::definition();
    assert_eq!(tool.name.as_ref(), "read");
    assert_eq!(tool.schema, Some(ReadTool::schema()));
}

#[tokio::test]
async fn whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "one\ntwo\nthree\n").unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content, "one\ntwo\nthree\n");
    assert_eq!(out.total_lines, 3);
    assert!(!out.truncated);
}

#[tokio::test]
async fn window() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "1\n2\n3\n4\n5\n").unwrap();

    let out = call(&f, Some(2), Some(2)).run().await.unwrap();
    assert_eq!(out.content, "2\n3\n");
    assert_eq!(out.total_lines, 5);
    assert!(out.truncated);

    let out = call(&f, Some(4), Some(10)).run().await.unwrap();
    assert_eq!(out.content, "4\n5\n");
    assert!(!out.truncated);

    // Window ends exactly at the last line.
    let out = call(&f, Some(4), Some(2)).run().await.unwrap();
    assert_eq!(out.content, "4\n5\n");
    assert!(!out.truncated);

    let out = call(&f, None, Some(5)).run().await.unwrap();
    assert_eq!(out.total_lines, 5);
    assert!(!out.truncated);
}

#[tokio::test]
async fn trailing_blank_lines() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "a\n\n\n").unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content, "a\n\n\n");
    assert_eq!(out.total_lines, 3);

    let out = call(&f, Some(3), None).run().await.unwrap();
    assert_eq!(out.content, "\n");
    assert!(!out.truncated);
}

// The only test in this binary that changes $HOME.
#[tokio::test]
async fn tilde_expands_to_home() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("t.txt"), "home\n").unwrap();
    std::env::set_var("HOME", home.path());

    let out = call(Path::new("~/t.txt"), None, None).run().await.unwrap();
    assert_eq!(out.content, "home\n");

    let err = call(Path::new("~"), None, None).run().await.unwrap_err();
    assert!(err.message.contains("directory"));

    // `~user` is not expanded.
    let err = call(Path::new("~nobody/t.txt"), None, None)
        .run()
        .await
        .unwrap_err();
    assert!(err.message.contains("~nobody/t.txt"));
}

#[tokio::test]
async fn default_limit() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.txt");
    let text: String = (1..=2500).map(|i| format!("{i}\n")).collect();
    std::fs::write(&f, text).unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content.lines().count(), DEFAULT_READ_LIMIT);
    assert_eq!(out.total_lines, 2500);
    assert!(out.truncated);
}

#[tokio::test]
async fn last_line_and_huge_limit() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "1\n2\n3\n").unwrap();

    let out = call(&f, Some(3), None).run().await.unwrap();
    assert_eq!(out.content, "3\n");
    assert!(!out.truncated);

    let out = call(&f, Some(2), Some(usize::MAX)).run().await.unwrap();
    assert_eq!(out.content, "2\n3\n");
    assert!(!out.truncated);
}

#[tokio::test]
async fn zero_limit_fails() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "1\n").unwrap();

    let err = call(&f, None, Some(0)).run().await.unwrap_err();
    assert!(err.message.contains("limit"));
}

#[tokio::test]
async fn exact_text() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");

    for (text, lines) in [
        ("a\r\nb\r\n", 2),
        ("a\nb", 2),
        ("a\r\nb\nc", 3),
        ("a\rb\n", 1),
        ("\n", 1),
        ("\n\n", 2),
    ] {
        std::fs::write(&f, text).unwrap();
        let out = call(&f, None, None).run().await.unwrap();
        assert_eq!(out.content, text, "{text:?}");
        assert_eq!(out.total_lines, lines, "{text:?}");
    }

    // A window keeps the endings of its own lines.
    std::fs::write(&f, "a\r\nb\nc").unwrap();
    let out = call(&f, Some(2), None).run().await.unwrap();
    assert_eq!(out.content, "b\nc");
    let out = call(&f, Some(1), Some(1)).run().await.unwrap();
    assert_eq!(out.content, "a\r\n");
}

#[tokio::test]
async fn bad_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "1\n2\n").unwrap();

    assert!(call(&f, Some(0), None).run().await.is_err());
    assert!(call(&f, Some(3), None).run().await.is_err());
}

#[tokio::test]
async fn missing_and_directory() {
    let dir = tempfile::tempdir().unwrap();

    let err = call(&dir.path().join("nope"), None, None)
        .run()
        .await
        .unwrap_err();
    assert!(err.message.contains("nope"));

    let err = call(dir.path(), None, None).run().await.unwrap_err();
    assert!(err.message.contains("directory"));

    assert!(call(Path::new(""), None, None).run().await.is_err());
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

    for path in [
        fifo.as_path(),
        Path::new("/dev/zero"),
        Path::new("/dev/null"),
    ] {
        let r = tokio::time::timeout(Duration::from_secs(5), call(path, None, None).run())
            .await
            .unwrap_or_else(|_| panic!("{} hung", path.display()));
        let err = r.unwrap_err();
        assert!(err.message.contains("not a regular file"), "{}", err);
    }
}

#[tokio::test]
async fn binary_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("bin");

    // NUL at the start, and far into the file.
    let mut late = vec![b'a'; 100_000];
    late.push(0);
    for bytes in [vec![0x7f, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00], late] {
        std::fs::write(&f, &bytes).unwrap();
        let err = call(&f, None, None).run().await.unwrap_err();
        assert!(err.message.contains("binary file"), "{err}");
        assert!(err.message.contains("bin"));
    }
}

#[tokio::test]
async fn invalid_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("bin");
    std::fs::write(&f, [0x66, 0xff, 0xfe, 0x0a, 0x67]).unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.total_lines, 2);
    assert!(out.content.contains('\u{FFFD}'));
}

#[tokio::test]
async fn empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("empty");
    std::fs::write(&f, "").unwrap();

    let out = call(&f, None, None).run().await.unwrap();
    assert_eq!(out.content, "");
    assert_eq!(out.total_lines, 0);
    assert!(!out.truncated);

    // Offset is not validated against an empty file.
    let out = call(&f, Some(5), None).run().await.unwrap();
    assert_eq!(out.content, "");
    assert_eq!(out.total_lines, 0);
}
