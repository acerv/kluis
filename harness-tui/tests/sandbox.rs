// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::process::{Command, Output, Stdio};

fn run_cli(args: &[&str]) -> Output {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    // Git set up as kl advises, so that it does not warn.
    let gitconfig = home.path().join("gitconfig");
    std::fs::write(
        &gitconfig,
        "[safe]\n\tbareRepository = explicit\n[core]\n\thooksPath = /dev/null\n",
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(args)
        .env("HOME", home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .env_remove("HARNESS_CONFIG")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn default_runs_sandboxed() {
    let out = run_cli(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sandboxed"), "{stdout}");
}

#[test]
fn apparmor_profile_refuses_a_binary_users_can_change() {
    let out = run_cli(&["--apparmor-profile"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("other than root"), "{stderr}");
}

#[test]
fn unknown_code_theme_is_refused_at_start() {
    let out = run_cli(&["--code-theme", "no-such-theme", "hi"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("code theme no-such-theme"), "{stderr}");
    assert!(stderr.contains("solarized-dark"), "{stderr}");
}

#[test]
fn no_sandbox_runs_directly() {
    let out = run_cli(&["--no-sandbox"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no sandbox"), "{stdout}");
}

#[test]
fn inner_exit_code_is_returned() {
    // The inner harness fails on the unknown model.
    let out = run_cli(&["--model", "no-such-model", "hi"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("error:"), "{stderr}");
    assert!(!stderr.contains("sandbox:"), "{stderr}");
}

#[test]
fn strict_runs_in_strict_sandbox() {
    let out = run_cli(&["--strict"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("strict sandbox"), "{stdout}");
}

#[test]
fn strict_conflicts_with_no_sandbox() {
    let out = run_cli(&["--strict", "--no-sandbox"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--no-sandbox"), "{stderr}");
}

#[test]
fn strict_checks_explicit_paths() {
    let out = run_cli(&["--strict", "--allow-read", "/no/such/folder"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("/no/such/folder"), "{stderr}");
}
