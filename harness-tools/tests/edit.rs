// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;

use common::p;
use harness_tools::{Edit, EditTool};

fn edit(old: &str, new: &str) -> Edit {
    Edit {
        old_text: old.into(),
        new_text: new.into(),
    }
}

fn call(path: &Path, edits: Vec<Edit>) -> EditTool {
    EditTool {
        path: p(path),
        edits,
    }
}

/// A temporary file holding `content`.
fn file(content: impl AsRef<[u8]>) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, content).unwrap();
    (dir, f)
}

fn read(f: &Path) -> String {
    std::fs::read_to_string(f).unwrap()
}

#[test]
fn definition() {
    let tool = EditTool::definition();
    assert_eq!(tool.name.as_ref(), "edit");
    assert_eq!(tool.schema, Some(EditTool::schema()));
}

#[test]
fn input_uses_camel_case() {
    let tool: EditTool = serde_json::from_value(serde_json::json!({
        "path": "a",
        "edits": [{"oldText": "x", "newText": "y"}]
    }))
    .unwrap();
    assert_eq!(tool.edits, [edit("x", "y")]);
}

#[tokio::test]
async fn single_edit() {
    let (_dir, f) = file("one\ntwo\nthree\n");

    let out = call(&f, vec![edit("two", "2")]).run().await.unwrap();
    assert_eq!(out.replacements, 1);
    assert_eq!(read(&f), "one\n2\nthree\n");
}

#[tokio::test]
async fn edits_match_the_original() {
    let (_dir, f) = file("a\nb\nc\n");

    // The second edit's text would not exist after the first one ran.
    let edits = vec![edit("c\n", "C\n"), edit("a\nb", "b\na")];
    let out = call(&f, edits).run().await.unwrap();
    assert_eq!(out.replacements, 2);
    assert_eq!(read(&f), "b\na\nC\n");
}

#[tokio::test]
async fn keeps_crlf() {
    let (_dir, f) = file("a\r\nb\r\nc\r\n");

    call(&f, vec![edit("a\nb", "x\ny\nz")]).run().await.unwrap();
    assert_eq!(read(&f), "x\r\ny\r\nz\r\nc\r\n");
}

#[tokio::test]
async fn keeps_crlf_on_last_line() {
    let (_dir, f) = file("a\r\nb");

    call(&f, vec![edit("b", "b\nc")]).run().await.unwrap();
    assert_eq!(read(&f), "a\r\nb\r\nc");
}

#[tokio::test]
async fn keeps_bom() {
    let (_dir, f) = file("\u{feff}hello\n");

    call(&f, vec![edit("hello", "bye")]).run().await.unwrap();
    assert_eq!(read(&f), "\u{feff}bye\n");
}

#[tokio::test]
async fn not_found() {
    let (_dir, f) = file("abc\n");

    let err = call(&f, vec![edit("xyz", "1")]).run().await.unwrap_err();
    assert!(err.message.contains("could not find the text"), "{err}");
}

#[tokio::test]
async fn not_unique() {
    let (_dir, f) = file("x\nx\nx\n");

    let err = call(&f, vec![edit("x", "y")]).run().await.unwrap_err();
    assert!(err.message.contains("found 3 occurrences"), "{err}");
}

#[tokio::test]
async fn overlapping_edits() {
    let (_dir, f) = file("abcdef\n");

    let err = call(&f, vec![edit("abcd", "1"), edit("cdef", "2")])
        .run()
        .await
        .unwrap_err();
    assert!(
        err.message.contains("edits[0] and edits[1] overlap"),
        "{err}"
    );
}

#[tokio::test]
async fn invalid_edits() {
    let (_dir, f) = file("abc\n");

    let err = call(&f, vec![]).run().await.unwrap_err();
    assert!(err.message.contains("at least one"), "{err}");

    let err = call(&f, vec![edit("", "x")]).run().await.unwrap_err();
    assert!(err.message.contains("must not be empty"), "{err}");

    let err = call(&f, vec![edit("abc", "abc")]).run().await.unwrap_err();
    assert!(err.message.contains("makes no change"), "{err}");
}

#[tokio::test]
async fn no_op_edit_in_batch() {
    let (_dir, f) = file("a\nb\n");

    let err = call(&f, vec![edit("a", "A"), edit("b", "b")])
        .run()
        .await
        .unwrap_err();
    assert!(err.message.contains("edits[1] makes no change"), "{err}");
    assert_eq!(read(&f), "a\nb\n");
}

#[tokio::test]
async fn overlapping_occurrences_not_unique() {
    let (_dir, f) = file("aaa\n");

    let err = call(&f, vec![edit("aa", "b")]).run().await.unwrap_err();
    assert!(err.message.contains("found 2 occurrences"), "{err}");
}

#[tokio::test]
async fn mixed_line_endings() {
    let (_dir, f) = file("a\nb\r\nc\n");

    let edits = vec![edit("b", "x\ny"), edit("c", "z\nw")];
    call(&f, edits).run().await.unwrap();
    assert_eq!(read(&f), "a\nx\r\ny\r\nz\nw\n");
}

#[tokio::test]
async fn removes_whole_crlf() {
    let (_dir, f) = file("a\r\nb\r\n");

    call(&f, vec![edit("a\n", "x")]).run().await.unwrap();
    assert_eq!(read(&f), "xb\r\n");
}

#[tokio::test]
async fn keeps_lone_cr() {
    let (_dir, f) = file("a\rb\r");

    call(&f, vec![edit("a\nb", "x\ny")]).run().await.unwrap();
    assert_eq!(read(&f), "x\ry\r");
}

#[tokio::test]
async fn failure_leaves_file_untouched() {
    let (_dir, f) = file("a\nb\n");

    let edits = vec![edit("a", "A"), edit("missing", "M")];
    let err = call(&f, edits).run().await.unwrap_err();
    assert!(err.message.contains("could not find edits[1]"), "{err}");
    assert_eq!(read(&f), "a\nb\n");
}

#[tokio::test]
async fn rejected_files() {
    let dir = tempfile::tempdir().unwrap();

    let err = call(&dir.path().join("missing"), vec![edit("a", "b")])
        .run()
        .await
        .unwrap_err();
    assert!(err.message.contains("missing"), "{err}");

    let err = call(dir.path(), vec![edit("a", "b")])
        .run()
        .await
        .unwrap_err();
    assert!(err.message.contains("is a directory"), "{err}");

    let (_d, f) = file([b'a', 0, b'b']);
    let err = call(&f, vec![edit("a", "b")]).run().await.unwrap_err();
    assert!(err.message.contains("binary file"), "{err}");

    let (_d, f) = file([b'a', 0xff, b'b']);
    let err = call(&f, vec![edit("a", "b")]).run().await.unwrap_err();
    assert!(err.message.contains("not valid UTF-8"), "{err}");
}
