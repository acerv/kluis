// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Run the harness with `home` as `$HOME` and no input.
fn run_cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(args)
        .env("HOME", home)
        .env_remove("HARNESS_CONFIG")
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Save an Anthropic login in `home` with `mode` permissions.
fn write_login(home: &Path, mode: u32) -> PathBuf {
    let dir = home.join(".kluis");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("oauth.toml");
    std::fs::write(&path, "[anthropic]\naccess_token = \"at\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

#[test]
fn login_prints_page_and_needs_code() {
    let home = tempfile::tempdir().unwrap();
    let out = run_cli(home.path(), &["--login", "anthropic"]);
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains("https://claude.ai/oauth/authorize?"),
        "{}",
        stdout(&out)
    );
    assert!(stderr(&out).contains("login cancelled"), "{}", stderr(&out));
    assert!(!home.path().join(".kluis/oauth.toml").exists());
}

#[test]
fn login_rejects_unknown_provider_and_prompt() {
    let home = tempfile::tempdir().unwrap();
    let out = run_cli(home.path(), &["--login", "nope"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("unknown login provider"),
        "{}",
        stderr(&out)
    );

    let out = run_cli(home.path(), &["--login", "anthropic", "hello"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("cannot be used"), "{}", stderr(&out));
}

#[test]
fn logout_removes_login() {
    let home = tempfile::tempdir().unwrap();
    let out = run_cli(home.path(), &["--logout", "anthropic"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Not logged in to anthropic"));

    let path = write_login(home.path(), 0o600);
    let out = run_cli(home.path(), &["--logout", "anthropic"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Logged out of anthropic"));
    assert!(!path.exists());
}

#[test]
fn open_login_file_is_rejected() {
    let home = tempfile::tempdir().unwrap();
    write_login(home.path(), 0o644);
    let out = run_cli(home.path(), &["--no-sandbox"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("chmod 600"), "{}", stderr(&out));
}
