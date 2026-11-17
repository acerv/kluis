// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::process::{Command, Output, Stdio};

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harness-cli"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn default_runs_sandboxed() {
    let out = run_cli(&[]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sandboxed"), "{stdout}");
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
