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

#[test]
fn paths_reach_the_sandbox() {
    let home = tempdir();
    let dir = tempdir();
    let dir = dir.path().to_str().unwrap();
    let missing = format!("{dir}/missing");

    for (key, path, ok) in [
        ("read", dir, true),
        ("write", dir, true),
        ("read", missing.as_str(), false),
        ("write", missing.as_str(), false),
    ] {
        write_default(home.path(), &format!("[sandbox]\n{key} = [\"{path}\"]\n"));
        let out = run_cli(home.path(), &[]);
        assert_eq!(out.status.success(), ok, "{key} {path}: {}", stderr(&out));
        if !ok {
            assert!(stderr(&out).contains(path), "{}", stderr(&out));
        }
    }
}

#[test]
fn tilde_is_home() {
    let home = tempdir();
    write_default(home.path(), "[sandbox]\nread = [\"~/missing\"]\n");
    let out = run_cli(home.path(), &[]);
    assert!(!out.status.success());
    let expected = home.path().join("missing");
    assert!(
        stderr(&out).contains(expected.to_str().unwrap()),
        "{}",
        stderr(&out)
    );
}

#[test]
fn relative_paths_are_rejected() {
    let home = tempdir();
    write_default(home.path(), "[sandbox]\nwrite = [\"src\"]\n");
    let out = run_cli(home.path(), &["--no-sandbox"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("src: not an absolute path"),
        "{}",
        stderr(&out)
    );
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn network_comes_from_file_or_flag() {
    let home = tempdir();
    for (text, args, network) in [
        ("", &[][..], false),
        ("[sandbox]\nnetwork = true\n", &[][..], true),
        ("[sandbox]\nnetwork = false\n", &["--network"][..], true),
    ] {
        write_default(home.path(), text);
        let out = run_cli(home.path(), args);
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(
            stdout(&out).contains("network"),
            network,
            "{}",
            stdout(&out)
        );
    }
}

#[test]
fn ports_are_accepted() {
    let home = tempdir();
    write_default(home.path(), "[sandbox]\nconnect = [11434, 8080]\n");
    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn file_in_current_folder_works() {
    // Shown read-only on top of the writable current folder.
    let home = tempdir();
    let cwd = tempdir();
    let sub = cwd.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("config.toml"), "[sandbox]\nenv = [\"CC\"]\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_kl-cli"))
        .args(["--config", "sub/config.toml"])
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HARNESS_CONFIG")
        .env_remove("HARNESS_SANDBOXED")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("sandboxed"), "{}", stdout(&out));
}

#[test]
fn file_behind_link_works() {
    // The link is not shown in the container, only the file.
    let home = tempdir();
    let dir = tempdir();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[sandbox]\nenv = [\"CC\"]\n").unwrap();
    let link = home.path().join("link.toml");
    std::os::unix::fs::symlink(&path, &link).unwrap();

    let out = run_cli(home.path(), &["--config", link.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn file_behind_unmounted_link_works() {
    // Neither the link's folder nor the target's folder is mounted.
    let home = tempdir();
    let link_dir = tempdir();
    let target_dir = tempdir();
    let target = target_dir.path().join("real.toml");
    std::fs::write(&target, "[sandbox]\nnetwork = true\n").unwrap();
    let link = link_dir.path().join("link.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let out = run_cli(home.path(), &["--config", link.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("network"), "{}", stdout(&out));
}

#[test]
fn no_sandbox_uses_given_config_even_if_marker_set() {
    let home = tempdir();
    let missing = home.path().join("explicit.toml");
    let out = Command::new(env!("CARGO_BIN_EXE_kl-cli"))
        .args(["--no-sandbox", "--config", missing.to_str().unwrap()])
        .env("HOME", home.path())
        .env("HARNESS_SANDBOXED", "1")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HARNESS_CONFIG")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("explicit.toml"), "{}", stderr(&out));
}
