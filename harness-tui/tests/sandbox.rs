// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::io::Write;
use std::process::{Command, Output, Stdio};

/// Git set up as kl advises, so that it does not warn.
const SAFE_GIT: &str = "[safe]\n\tbareRepository = explicit\n[core]\n\thooksPath = /dev/null\n";

fn run_cli(args: &[&str]) -> Output {
    run_cli_with_git(args, SAFE_GIT, "").0
}

/// Run `kl` with `args`, `gitconfig` as the only git configuration and
/// `input` on its standard input. Also returns the git configuration after.
fn run_cli_with_git(args: &[&str], gitconfig: &str, input: &str) -> (Output, String) {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let config = home.path().join("gitconfig");
    std::fs::write(&config, gitconfig).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(args)
        .env("HOME", home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &config)
        .env_remove("HARNESS_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (out, std::fs::read_to_string(&config).unwrap())
}

#[test]
fn setup_git_sets_bare_repository_when_confirmed() {
    let (out, config) = run_cli_with_git(&["--setup-git"], "", "y\n");
    assert!(out.status.success(), "{out:?}");
    assert!(config.contains("bareRepository = explicit"), "{config}");
}

#[test]
fn setup_git_changes_nothing_unless_confirmed() {
    let (out, config) = run_cli_with_git(&["--setup-git"], "", "\n");
    assert!(out.status.success(), "{out:?}");
    assert!(config.is_empty(), "{config}");
}

#[test]
fn setup_git_does_not_ask_when_already_set() {
    let (out, _) = run_cli_with_git(&["--setup-git"], SAFE_GIT, "");
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("already"), "{stdout}");
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
