// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::process::{Command, Output, Stdio};

use common::{kl, tempdir};

fn run_cli(args: &[&str]) -> Output {
    let home = tempdir();
    // Git set up as kl advises, so that it does not warn.
    let gitconfig = home.path().join("gitconfig");
    std::fs::write(
        &gitconfig,
        "[safe]\n\tbareRepository = explicit\n[core]\n\thooksPath = /dev/null\n",
    )
    .unwrap();
    kl(home.path())
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
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

#[test]
fn refuses_to_run_as_root() {
    for args in [&[][..], &["--no-sandbox"]] {
        // Root in a user namespace of its own, which needs no privilege.
        let out = Command::new("unshare")
            .arg("--map-root-user")
            .arg(env!("CARGO_BIN_EXE_kl"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?}");
        assert!(stderr.contains("refusing to run as root"), "{stderr}");
    }
}

#[test]
fn sandbox_info_prints_what_commands_get() {
    let args = [
        "--sandbox-info",
        "--allow-connect",
        "5432",
        "--max-memory",
        "2G",
        "--web-host",
        "docs.rs",
        "--pass-env",
        "CARGO_MANIFEST_DIR",
    ];
    let out = run_cli(&args);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let cwd = std::env::current_dir().unwrap();
    let changed = format!("Commands can change:\n  {}\n", cwd.display());
    assert!(stdout.contains(&changed), "{stdout}");
    assert!(stdout.contains("\n  /usr\n"), "{stdout}");
    for line in [
        "Network: TCP ports 5432 on any host",
        "Memory of a command: at most 2.0 GiB",
        "Web tools, which run outside the sandbox: web_fetch, only docs.rs",
    ] {
        assert!(stdout.contains(line), "{stdout}");
    }
    let env = stdout
        .lines()
        .find(|line| line.starts_with("Environment variables passed: "))
        .unwrap();
    assert!(env.contains("CARGO_MANIFEST_DIR"), "{env}");

    // With --strict nothing of the project can be changed.
    let out = run_cli(&["--sandbox-info", "--strict"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("Commands can change:"), "{stdout}");
}

#[test]
fn sandbox_info_tells_when_there_is_no_sandbox() {
    let out = run_cli(&["--sandbox-info", "--no-sandbox"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("Sandbox: off."), "{stdout}");
    assert!(stdout.contains("web_fetch and web_search"), "{stdout}");
}

#[test]
fn sandbox_info_takes_no_prompt() {
    let out = run_cli(&["--sandbox-info", "hello"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}
