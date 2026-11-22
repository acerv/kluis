// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;

use common::p;
use harness_exec::{FindTool, MAX_FIND_BYTES};

fn call(pattern: &str, path: &Path) -> FindTool {
    FindTool {
        pattern: pattern.into(),
        path: Some(p(path)),
        limit: None,
    }
}

/// Create empty files, and their folders, below `root`.
fn touch(root: &Path, files: &[&str]) {
    for file in files {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
}

async fn find(pattern: &str, root: &Path) -> Vec<String> {
    call(pattern, root).run().await.unwrap().paths
}

#[test]
fn definition() {
    let tool = FindTool::definition();
    assert_eq!(tool.name.as_ref(), "find");
    assert_eq!(tool.schema, Some(FindTool::schema()));
}

#[tokio::test]
async fn name_pattern_matches_at_any_depth() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &["a.rs", "b.txt", "src/c.rs", "src/deep/d.rs"]);

    let out = call("*.rs", dir.path()).run().await.unwrap();
    assert_eq!(out.paths, ["a.rs", "src/c.rs", "src/deep/d.rs"]);
    assert_eq!(out.total, 3);
    assert!(!out.truncated);
}

#[tokio::test]
async fn path_pattern_starts_at_root() {
    let dir = tempfile::tempdir().unwrap();
    touch(
        dir.path(),
        &["src/a.rs", "src/deep/b.rs", "other/src/c.rs", "top.rs"],
    );

    assert_eq!(
        find("src/**/*.rs", dir.path()).await,
        ["src/a.rs", "src/deep/b.rs"]
    );
    // `*` does not cross folders.
    assert_eq!(find("src/*.rs", dir.path()).await, ["src/a.rs"]);
    assert_eq!(find("./src/*.rs", dir.path()).await, ["src/a.rs"]);
}

#[tokio::test]
async fn double_star_patterns() {
    let dir = tempfile::tempdir().unwrap();
    touch(
        dir.path(),
        &["a.rs", "src/b.rs", "lib/src/c.rs", "src/x/d.txt"],
    );

    // A leading `**/` also matches at the top: same as a name pattern.
    assert_eq!(
        find("**/*.rs", dir.path()).await,
        find("*.rs", dir.path()).await
    );
    assert_eq!(
        find("**/src/*.rs", dir.path()).await,
        ["lib/src/c.rs", "src/b.rs"]
    );
    // Everything below src, but not src itself.
    assert_eq!(
        find("src/**", dir.path()).await,
        ["src/b.rs", "src/x/", "src/x/d.txt"]
    );
    // Every file and folder: 4 of each.
    assert_eq!(find("**", dir.path()).await.len(), 8);
}

#[tokio::test]
async fn directories_end_with_slash() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &["src/a.rs", "lib/src/b.rs"]);

    assert_eq!(find("src", dir.path()).await, ["lib/src/", "src/"]);
}

#[tokio::test]
async fn smart_case() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &["README.md", "readme.txt"]);

    assert_eq!(
        find("readme*", dir.path()).await,
        ["README.md", "readme.txt"]
    );
    assert_eq!(find("README*", dir.path()).await, ["README.md"]);
}

#[tokio::test]
async fn hidden_files_but_not_git() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &[".env", ".config/x", ".git/config", "a"]);

    assert_eq!(
        find("*", dir.path()).await,
        [".config/", ".config/x", ".env", "a"]
    );
}

#[tokio::test]
async fn gitignore_in_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &[".git/HEAD", "src/a.rs", "target/b.rs"]);
    std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();

    assert_eq!(find("*.rs", dir.path()).await, ["src/a.rs"]);
}

#[tokio::test]
async fn nested_repository_stops_outer_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    touch(
        dir.path(),
        &[".git/HEAD", "a.log", "sub/.git/HEAD", "sub/b.log"],
    );
    std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();

    assert_eq!(find("*.log", dir.path()).await, ["sub/b.log"]);
}

#[tokio::test]
async fn gitignore_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &["a.rs", "debug.log"]);
    std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();

    assert_eq!(find("*", dir.path()).await, [".gitignore", "a.rs"]);
}

#[tokio::test]
async fn symlinks_are_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &["real/a.rs"]);
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();

    assert_eq!(find("*", dir.path()).await, ["link", "real/", "real/a.rs"]);
}

#[tokio::test]
async fn limit_truncates_but_counts_all() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &["1", "2", "3", "4", "5"]);

    let out = FindTool {
        limit: Some(2),
        ..call("*", dir.path())
    }
    .run()
    .await
    .unwrap();
    assert_eq!(out.paths, ["1", "2"]);
    assert_eq!(out.total, 5);
    assert!(out.truncated);
}

#[tokio::test]
async fn byte_limit_truncates() {
    let dir = tempfile::tempdir().unwrap();
    // 100 bytes per path, so MAX_FIND_BYTES holds far fewer than the 600 created.
    let name = "x".repeat(96);
    let files: Vec<String> = (0..600).map(|i| format!("{name}{i:04}")).collect();
    touch(
        dir.path(),
        &files.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    let out = FindTool {
        limit: Some(1000),
        ..call("*", dir.path())
    }
    .run()
    .await
    .unwrap();
    assert_eq!(out.total, 600);
    assert!(out.truncated);
    let bytes: usize = out.paths.iter().map(String::len).sum::<usize>() + out.paths.len();
    assert!(bytes <= MAX_FIND_BYTES, "{bytes}");
}

#[tokio::test]
async fn rejects_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    touch(dir.path(), &["a.txt"]);

    let err = |tool: FindTool| async { tool.run().await.unwrap_err().message };
    let zero = FindTool {
        limit: Some(0),
        ..call("*", dir.path())
    };
    assert!(err(zero).await.contains("limit must be >= 1"));
    assert!(err(call("", dir.path()))
        .await
        .contains("must not be empty"));
    assert!(err(call("[", dir.path())).await.contains("invalid pattern"));
    assert!(err(call("*", &file)).await.contains("not a directory"));
    let missing = dir.path().join("nope");
    assert!(err(call("*", &missing))
        .await
        .contains("No such file or directory"));
}

#[tokio::test]
async fn relative_and_default_paths_start_at_cwd() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), &["sub/a.rs", "b.rs"]);

    let tool = |path: Option<&str>| FindTool {
        pattern: "*.rs".into(),
        path: path.map(Into::into),
        limit: None,
    };
    let out = tool(Some("sub")).run_in(dir.path()).await.unwrap();
    assert_eq!(out.paths, ["a.rs"]);
    let out = tool(None).run_in(dir.path()).await.unwrap();
    assert_eq!(out.paths, ["b.rs", "sub/a.rs"]);
    let out = tool(Some("")).run_in(dir.path()).await.unwrap();
    assert_eq!(out.paths, ["b.rs", "sub/a.rs"]);
}
