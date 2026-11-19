// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;

use common::p;
use harness_tools::{DiffHunk, DiffLine, Edit, EditTool, MAX_DIFF_BYTES, MAX_EDIT_BYTES};

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

#[tokio::test]
async fn relative_path_starts_at_cwd() {
    let (dir, f) = file("old\n");

    let out = call(Path::new("a.txt"), vec![edit("old", "new")])
        .run_in(dir.path())
        .await
        .unwrap();
    assert_eq!(out.replacements, 1);
    assert_eq!(read(&f), "new\n");
}

#[tokio::test]
async fn preserves_mode_and_replaces_atomically() {
    use std::fs::Permissions;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (dir, f) = file("old\n");
    std::fs::set_permissions(&f, Permissions::from_mode(0o755)).unwrap();
    let old_ino = std::fs::metadata(&f).unwrap().ino();

    let out = call(&f, vec![edit("old", "new")]).run().await.unwrap();
    assert_eq!(out.replacements, 1);

    let meta = std::fs::metadata(&f).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o755);
    assert_ne!(meta.ino(), old_ino);
    assert_eq!(read(&f), "new\n");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn rejects_file_over_max_edit_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("huge.txt");

    // A sparse file over the cap is rejected before reading (even though its
    // contents are NUL bytes).
    let file = std::fs::File::create(&f).unwrap();
    file.set_len(MAX_EDIT_BYTES + 1).unwrap();
    drop(file);

    let err = call(&f, vec![edit("a", "b")]).run().await.unwrap_err();
    assert!(err.message.contains("larger than"), "{err}");
    assert!(err.message.contains("MB"), "{err}");
}

#[tokio::test]
async fn diff_for_single_edit_and_serialization() {
    let (_dir, f) = file("one\ntwo\nthree\n");

    let out = call(&f, vec![edit("two", "2")]).run().await.unwrap();
    assert_eq!(out.diff.first_changed_line, Some(2));
    assert!(!out.diff.truncated);
    assert_eq!(
        out.diff.hunks,
        vec![DiffHunk {
            old_start: 1,
            old_lines: 3,
            new_start: 1,
            new_lines: 3,
            lines: vec![
                DiffLine::Context("one".into()),
                DiffLine::Removed("two".into()),
                DiffLine::Added("2".into()),
                DiffLine::Context("three".into()),
            ],
        }]
    );
    assert_eq!(
        out.diff.to_string(),
        "@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n"
    );
    // Serializing for the model omits the UI diff.
    assert_eq!(
        serde_json::to_value(&out).unwrap(),
        serde_json::json!({ "replacements": 1 })
    );
}

#[tokio::test]
async fn diff_collapses_unchanged_context_in_old_text() {
    let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    let (_dir, f) = file(&content);

    // Padded with unchanged lines around line 5 for uniqueness.
    let old = "line 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8";
    let new = "line 2\nline 3\nline 4\nLINE 5\nline 6\nline 7\nline 8";
    let out = call(&f, vec![edit(old, new)]).run().await.unwrap();

    assert_eq!(out.diff.first_changed_line, Some(5));
    assert_eq!(
        out.diff.to_string(),
        "@@ -2,7 +2,7 @@\n line 2\n line 3\n line 4\n-line 5\n+LINE 5\n line 6\n line 7\n line 8\n"
    );
}

#[tokio::test]
async fn diff_distant_and_nearby_edits() {
    // Distant edits produce separate hunks with line offsets updated by earlier hunks.
    let content: String = (1..=25).map(|i| format!("L{i:02}\n")).collect();
    let (_dir, f) = file(&content);

    let edits = vec![edit("L02", "L02a\nL02b\nL02c"), edit("L20", "L20_new")];
    let out = call(&f, edits).run().await.unwrap();
    assert_eq!(out.diff.first_changed_line, Some(2));
    assert_eq!(out.diff.hunks.len(), 2);
    assert_eq!(out.diff.hunks[0].old_start, 1);
    assert_eq!(out.diff.hunks[0].old_lines, 5);
    assert_eq!(out.diff.hunks[0].new_start, 1);
    assert_eq!(out.diff.hunks[0].new_lines, 7);
    // Hunk 2 starts 3 lines before L20 (old line 17, shifted +2 to new line 19).
    assert_eq!(out.diff.hunks[1].old_start, 17);
    assert_eq!(out.diff.hunks[1].new_start, 19);

    // Nearby edits merge into one hunk.
    let (_dir2, f2) = file(&content);
    let out = call(&f2, vec![edit("L03", "THREE"), edit("L07", "SEVEN")])
        .run()
        .await
        .unwrap();
    assert_eq!(out.diff.hunks.len(), 1);
    assert_eq!(out.diff.hunks[0].old_start, 1);
    assert_eq!(out.diff.hunks[0].old_lines, 10);
}

#[tokio::test]
async fn diff_joins_lines_and_handles_full_deletion() {
    // Removing a newline merges the edited line with the line after it.
    let (_dir, f) = file("a\nb\nc\n");
    let out = call(&f, vec![edit("b\n", "B")]).run().await.unwrap();
    assert_eq!(out.diff.first_changed_line, Some(2));
    assert_eq!(out.diff.to_string(), "@@ -1,3 +1,2 @@\n a\n-b\n-c\n+Bc\n");

    // Deleting every line produces an empty new range.
    let (_dir2, f2) = file("a\nb\n");
    let out = call(&f2, vec![edit("a\nb\n", "")]).run().await.unwrap();
    assert_eq!(out.diff.first_changed_line, Some(1));
    assert_eq!(out.diff.to_string(), "@@ -1,2 +0,0 @@\n-a\n-b\n");
}

#[tokio::test]
async fn diff_truncates_at_max_diff_bytes() {
    // ~200 KiB old + ~200 KiB new exceeds MAX_DIFF_BYTES (256 KiB).
    let old: String = (0..2000)
        .map(|i| format!("old-{i:04}-{}\n", "a".repeat(90)))
        .collect();
    let new: String = (0..2000)
        .map(|i| format!("new-{i:04}-{}\n", "b".repeat(90)))
        .collect();
    let (_dir, f) = file(&old);

    let out = call(&f, vec![edit(&old, &new)]).run().await.unwrap();
    assert!(out.diff.truncated);
    assert_eq!(out.diff.first_changed_line, Some(1));
    let total_bytes: usize = out
        .diff
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| match l {
            DiffLine::Context(s) | DiffLine::Removed(s) | DiffLine::Added(s) => s.len() + 1,
        })
        .sum();
    assert!(total_bytes <= MAX_DIFF_BYTES);
    assert_eq!(read(&f), new);

    // Truncating before the first changed line does not emit a context-only hunk.
    let huge = "x".repeat(MAX_DIFF_BYTES + 10);
    let (_dir2, f2) = file(format!("ctx1\nctx2\n{huge}\n"));
    let out = call(&f2, vec![edit(&huge, "small")]).run().await.unwrap();
    assert!(out.diff.truncated);
    assert_eq!(out.diff.first_changed_line, Some(3));
    assert!(out.diff.hunks.is_empty());
}
