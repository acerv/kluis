// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// A folder below the target folder: outside `/tmp`, which is private in
/// the container, and outside the current folder, which it shows.
fn tempdir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// Run the harness with `home` as `$HOME` and no `$XDG_CONFIG_HOME`.
fn run_cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kl-cli"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HARNESS_CONFIG")
        .env_remove("HARNESS_SANDBOXED")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Write `text` as the default config file of `home`.
fn write_default(home: &Path, text: &str) -> PathBuf {
    let dir = home.join(".config/harness");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn missing_default_file_is_fine() {
    let home = tempdir();
    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn default_file_is_read() {
    let home = tempdir();
    let path = write_default(home.path(), "[sandbox]\nenv = [\"CC\"]\n");
    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    std::fs::write(&path, "[sandbox]\nenv = \"CC\"\n").unwrap();
    let out = run_cli(home.path(), &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(path.to_str().unwrap()),
        "{}",
        stderr(&out)
    );
}

#[test]
fn xdg_config_home_is_used() {
    let home = tempdir();
    let xdg = tempdir();
    let dir = xdg.path().join("harness");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "not toml").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_kl-cli"))
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", xdg.path())
        .env_remove("HARNESS_CONFIG")
        .env_remove("HARNESS_SANDBOXED")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("config.toml"), "{}", stderr(&out));
}

#[test]
fn unknown_keys_are_rejected() {
    let home = tempdir();
    write_default(home.path(), "[sandbox]\nnetwrok = true\n");
    let out = run_cli(home.path(), &["--no-sandbox"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("netwrok"), "{}", stderr(&out));
}

#[test]
fn missing_given_file_fails() {
    let home = tempdir();
    let path = home.path().join("missing.toml");
    let out = run_cli(home.path(), &["--config", path.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("missing.toml"), "{}", stderr(&out));
}

#[test]
fn given_file_is_readable_in_the_sandbox() {
    // Neither below $HOME nor the current folder: only visible in the
    // container if the harness shows it there.
    let home = tempdir();
    let dir = tempdir();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[sandbox]\nenv = [\"CC\"]\n").unwrap();

    let out = run_cli(home.path(), &["--config", path.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("sandboxed"));
}
