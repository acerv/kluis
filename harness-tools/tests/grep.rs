// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;

use common::p;
use harness_tools::{GrepTool, MAX_GREP_BYTES, MAX_GREP_LINE_CHARS};

fn call(pattern: &str, path: &Path) -> GrepTool {
    GrepTool {
        pattern: pattern.into(),
        path: Some(p(path)),
        glob: None,
        ignore_case: None,
        literal: None,
        context: None,
        limit: None,
    }
}

/// Create files below `root`, with their folders.
fn files(root: &Path, files: &[(&str, &str)]) {
    for (name, content) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

async fn grep(tool: GrepTool) -> String {
    tool.run().await.unwrap().output
}

#[test]
fn definition() {
    let tool = GrepTool::definition();
    assert_eq!(tool.name.as_ref(), "grep");
    assert_eq!(tool.schema, Some(GrepTool::schema()));
}

#[test]
fn camel_case_input() {
    let tool: GrepTool =
        serde_json::from_value(serde_json::json!({ "pattern": "x", "ignoreCase": true })).unwrap();
    assert_eq!(tool.ignore_case, Some(true));
}

#[tokio::test]
async fn matches_in_file_order() {
    let dir = tempfile::tempdir().unwrap();
    files(
        dir.path(),
        &[
            ("b.txt", "hello\nworld\nhello again\n"),
            ("A.txt", "say hello\n"),
            ("sub/c.txt", "nothing\n"),
        ],
    );

    let out = call("hello", dir.path()).run().await.unwrap();
    assert_eq!(
        out.output,
        "A.txt:1: say hello\nb.txt:1: hello\nb.txt:3: hello again"
    );
    assert_eq!(out.matches, 3);
    assert!(!out.truncated);
}

#[tokio::test]
async fn no_match() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), &[("a.txt", "hello\n")]);

    let out = call("absent", dir.path()).run().await.unwrap();
    assert_eq!(out.output, "");
    assert_eq!(out.matches, 0);
    assert!(!out.truncated);
}

#[tokio::test]
async fn regex_literal_and_case() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), &[("a.txt", "abc\na.c\nABC\n")]);

    assert_eq!(
        grep(call("a.c", dir.path())).await,
        "a.txt:1: abc\na.txt:2: a.c"
    );
    let literal = GrepTool {
        literal: Some(true),
        ..call("a.c", dir.path())
    };
    assert_eq!(grep(literal).await, "a.txt:2: a.c");
    let ignore_case = GrepTool {
        ignore_case: Some(true),
        ..call("abc", dir.path())
    };
    assert_eq!(grep(ignore_case).await, "a.txt:1: abc\na.txt:3: ABC");
}

#[tokio::test]
async fn context_lines() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), &[("f", "1\n2\n3\nhit\n5\n6\n7\n")]);

    let tool = GrepTool {
        context: Some(1),
        ..call("hit", dir.path())
    };
    assert_eq!(grep(tool).await, "f-3- 3\nf:4: hit\nf-5- 5");
}

#[tokio::test]
async fn limit_counts_matching_lines() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), &[("f", "m\nm\nm\n")]);

    let limited = |limit| GrepTool {
        limit: Some(limit),
        ..call("m", dir.path())
    };
    let out = limited(2).run().await.unwrap();
    assert_eq!(out.output, "f:1: m\nf:2: m");
    assert_eq!(out.matches, 2);
    assert!(out.truncated);

    // Exactly as many matches as the limit: nothing left out.
    let out = limited(3).run().await.unwrap();
    assert_eq!(out.matches, 3);
    assert!(!out.truncated);
}

#[tokio::test]
async fn limit_keeps_context_of_last_match() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), &[("f", "m\nm\nx\ny\nm\n")]);

    let tool = GrepTool {
        context: Some(2),
        limit: Some(1),
        ..call("m", dir.path())
    };
    let out = tool.run().await.unwrap();
    // A match in the context of the last one shows as context.
    assert_eq!(out.output, "f:1: m\nf-2- m\nf-3- x");
    assert_eq!(out.matches, 1);
    assert!(out.truncated);
}

#[tokio::test]
async fn limit_across_files() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), &[("a", "m\n"), ("b", "m\n")]);

    let tool = GrepTool {
        limit: Some(1),
        ..call("m", dir.path())
    };
    let out = tool.run().await.unwrap();
    // Which match is kept depends on the search order.
    assert!(
        ["a:1: m", "b:1: m"].contains(&out.output.as_str()),
        "{}",
        out.output
    );
    assert_eq!(out.matches, 1);
    assert!(out.truncated);
}

#[tokio::test]
async fn limit_across_many_files() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..200 {
        files(dir.path(), &[(&format!("d{}/f{i:03}", i % 7), "m\n")]);
    }

    let tool = GrepTool {
        limit: Some(50),
        ..call("m", dir.path())
    };
    let out = tool.run().await.unwrap();
    let lines: Vec<_> = out.output.lines().collect();
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(lines, sorted);
    assert_eq!(lines.len(), 50);
    assert_eq!(out.matches, 50);
    assert!(out.truncated);
}

#[tokio::test]
async fn files_come_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..500).map(|i| format!("d{}/f{i:03}", i % 7)).collect();
    for name in &names {
        files(dir.path(), &[(name, "m\n")]);
    }

    let tool = GrepTool {
        limit: Some(1000),
        ..call("m", dir.path())
    };
    let out = tool.run().await.unwrap();
    let mut expected: Vec<String> = names.iter().map(|n| format!("{n}:1: m")).collect();
    expected.sort();
    assert_eq!(out.output.lines().collect::<Vec<_>>(), expected);
    assert_eq!(out.matches, 500);
}

#[tokio::test]
async fn glob_filters_files() {
    let dir = tempfile::tempdir().unwrap();
    files(
        dir.path(),
        &[
            ("a.rs", "m\n"),
            ("b.txt", "m\n"),
            ("src/c.rs", "m\n"),
            ("x/src/d.rs", "m\n"),
        ],
    );
    let with = |glob: &str| GrepTool {
        glob: Some(glob.into()),
        ..call("m", dir.path())
    };

    assert_eq!(
        grep(with("*.rs")).await,
        "a.rs:1: m\nsrc/c.rs:1: m\nx/src/d.rs:1: m"
    );
    assert_eq!(grep(with("!*.rs")).await, "b.txt:1: m");
    assert_eq!(grep(with("src/*.rs")).await, "src/c.rs:1: m");
    assert_eq!(grep(with("./src/*.rs")).await, "src/c.rs:1: m");
    assert_eq!(
        grep(with("**/src/*.rs")).await,
        "src/c.rs:1: m\nx/src/d.rs:1: m"
    );
}

#[tokio::test]
async fn walks_like_find() {
    let dir = tempfile::tempdir().unwrap();
    files(
        dir.path(),
        &[
            (".env", "m\n"),
            (".git/config", "m\n"),
            (".gitignore", "*.log\n"),
            ("debug.log", "m\n"),
            ("bin", "m\0\n"),
        ],
    );

    assert_eq!(grep(call("m", dir.path())).await, ".env:1: m");
}

#[tokio::test]
async fn single_file_shows_as_given() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), &[("sub/a.log", "m\n")]);

    // Neither .gitignore rules nor the glob apply to a file asked for.
    std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
    let tool = GrepTool {
        path: Some("sub/a.log".into()),
        glob: Some("*.rs".into()),
        ..call("m", dir.path())
    };
    let out = tool.run_in(dir.path()).await.unwrap();
    assert_eq!(out.output, "sub/a.log:1: m");
}

#[tokio::test]
async fn line_endings_and_long_lines() {
    let dir = tempfile::tempdir().unwrap();
    let long = "m".repeat(MAX_GREP_LINE_CHARS + 100);
    files(dir.path(), &[("a", "m\r\n"), ("b", &long)]);

    let out = grep(call("m", dir.path())).await;
    let cut = format!("{}... [truncated]", "m".repeat(MAX_GREP_LINE_CHARS));
    assert_eq!(out, format!("a:1: m\nb:1: {cut}"));
}

#[tokio::test]
async fn byte_limit_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let line = format!("{}\n", "m".repeat(200));
    files(dir.path(), &[("f", &line.repeat(1000))]);

    let tool = GrepTool {
        limit: Some(1000),
        ..call("m", dir.path())
    };
    let out = tool.run().await.unwrap();
    assert!(out.output.len() <= MAX_GREP_BYTES, "{}", out.output.len());
    assert!(out.matches < 1000, "{}", out.matches);
    assert_eq!(out.output.lines().count(), out.matches);
    assert!(out.truncated);
}

#[tokio::test]
async fn rejects_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let err = |tool: GrepTool| async { tool.run().await.unwrap_err().message };

    let zero = GrepTool {
        limit: Some(0),
        ..call("m", dir.path())
    };
    assert!(err(zero).await.contains("limit must be >= 1"));
    assert!(err(call("(", dir.path())).await.contains("invalid pattern"));
    let bad_glob = GrepTool {
        glob: Some("[".into()),
        ..call("m", dir.path())
    };
    assert!(err(bad_glob).await.contains("invalid glob"));
    let missing = dir.path().join("nope");
    assert!(err(call("m", &missing))
        .await
        .contains("No such file or directory"));
}

#[tokio::test]
async fn relative_and_default_paths_start_at_cwd() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), &[("sub/a", "m\n"), ("b", "m\n")]);

    let tool = |path: Option<&str>| GrepTool {
        path: path.map(Into::into),
        ..call("m", dir.path())
    };
    let out = tool(Some("sub")).run_in(dir.path()).await.unwrap();
    assert_eq!(out.output, "a:1: m");
    let out = tool(None).run_in(dir.path()).await.unwrap();
    assert_eq!(out.output, "b:1: m\nsub/a:1: m");
}
