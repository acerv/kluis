// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;

use common::p;
use harness_tools::{LsTool, MAX_LS_BYTES};

fn call(path: impl Into<String>) -> LsTool {
    LsTool {
        path: Some(path.into()),
        limit: None,
    }
}

#[test]
fn definition() {
    let tool = LsTool::definition();
    assert_eq!(tool.name.as_ref(), "ls");
    assert_eq!(tool.schema, Some(LsTool::schema()));
}

#[tokio::test]
async fn empty_directory() {
    let dir = tempfile::tempdir().unwrap();

    let out = call(p(dir.path())).run().await.unwrap();
    assert!(out.entries.is_empty());
    assert_eq!(out.total, 0);
    assert!(!out.truncated);
}

#[tokio::test]
async fn sorted_ignoring_case_with_dotfiles() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["b.txt", ".hidden", "a.txt", "Z.txt"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }

    let out = call(p(dir.path())).run().await.unwrap();
    assert_eq!(out.entries, [".hidden", "a.txt", "b.txt", "Z.txt"]);
    assert_eq!(out.total, 4);
    assert!(!out.truncated);
}

#[tokio::test]
async fn case_only_differences_are_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "A.txt"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }

    let out = call(p(dir.path())).run().await.unwrap();
    assert_eq!(out.entries, ["A.txt", "a.txt"]);
}

#[tokio::test]
async fn directories_end_with_slash() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("main.rs"), "").unwrap();

    let out = call(p(dir.path())).run().await.unwrap();
    assert_eq!(out.entries, ["main.rs", "src/"]);
}

#[tokio::test]
async fn symlinks_are_followed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("broken")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("loop"), dir.path().join("loop")).unwrap();
    std::fs::write(dir.path().join("file"), "").unwrap();
    std::os::unix::fs::symlink(dir.path().join("file/x"), dir.path().join("through")).unwrap();

    let out = call(p(dir.path())).run().await.unwrap();
    assert_eq!(
        out.entries,
        ["broken@", "file", "link/", "loop@", "real/", "through@"]
    );
}

#[tokio::test]
async fn unreachable_target_is_not_broken() {
    // Permissions do not stop root.
    if nix::unistd::Uid::effective().is_root() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    let list = dir.path().join("list");
    std::fs::create_dir_all(locked.join("sub")).unwrap();
    std::fs::create_dir(&list).unwrap();
    std::os::unix::fs::symlink(locked.join("sub"), list.join("denied")).unwrap();
    std::fs::set_permissions(&locked, Permissions::from_mode(0o000)).unwrap();

    let out = call(p(&list)).run().await;
    std::fs::set_permissions(&locked, Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.unwrap().entries, ["denied"]);
}

#[tokio::test]
async fn empty_path_is_current_directory() {
    let empty = call("").run().await.unwrap();
    let dot = call(".").run().await.unwrap();
    assert_eq!(empty, dot);
}

#[tokio::test]
async fn limit_truncates_but_counts_all() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["1", "2", "3", "4", "5"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }

    let out = LsTool {
        path: Some(p(dir.path())),
        limit: Some(2),
    }
    .run()
    .await
    .unwrap();
    assert_eq!(out.entries, ["1", "2"]);
    assert_eq!(out.total, 5);
    assert!(out.truncated);
}

#[tokio::test]
async fn byte_limit_truncates() {
    let dir = tempfile::tempdir().unwrap();
    // 100 bytes per name, so 50 KB holds far fewer than the 600 created.
    let name = "x".repeat(96);
    for i in 0..600 {
        std::fs::write(dir.path().join(format!("{name}{i:04}")), "").unwrap();
    }

    let out = LsTool {
        path: Some(p(dir.path())),
        limit: Some(1000),
    }
    .run()
    .await
    .unwrap();
    assert_eq!(out.total, 600);
    assert!(out.truncated);
    assert!(out.entries.len() < 600, "{}", out.entries.len());
    let bytes: usize = out.entries.iter().map(String::len).sum::<usize>() + out.entries.len();
    assert!(bytes <= MAX_LS_BYTES, "{bytes}");
}

#[tokio::test]
async fn rejects_zero_limit() {
    let dir = tempfile::tempdir().unwrap();

    let err = LsTool {
        path: Some(p(dir.path())),
        limit: Some(0),
    }
    .run()
    .await
    .unwrap_err();
    assert!(err.message.contains("limit must be >= 1"), "{err}");
}

#[tokio::test]
async fn rejects_files_and_missing_paths() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "").unwrap();

    let err = call(p(&file)).run().await.unwrap_err();
    assert!(err.message.contains("not a directory"), "{err}");

    let missing = dir.path().join("nope");
    let err = call(p(&missing)).run().await.unwrap_err();
    assert!(err.message.contains("No such file or directory"), "{err}");
}

// The only test in this binary that changes $HOME.
#[tokio::test]
async fn tilde_expands_to_home() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("f.txt"), "").unwrap();
    std::env::set_var("HOME", home.path());

    let out = call("~").run().await.unwrap();
    assert_eq!(out.entries, ["f.txt"]);
}

#[tokio::test]
async fn relative_and_default_paths_start_at_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/f.txt"), "").unwrap();

    let out = call("sub").run_in(dir.path()).await.unwrap();
    assert_eq!(out.entries, ["f.txt"]);

    let out = LsTool {
        path: None,
        limit: None,
    }
    .run_in(dir.path())
    .await
    .unwrap();
    assert_eq!(out.entries, ["sub/"]);
}
