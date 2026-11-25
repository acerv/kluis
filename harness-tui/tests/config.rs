// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use common::{kl, stderr, tempdir};

/// Run the harness with `home` as `$HOME`.
fn run_cli(home: &Path, args: &[&str]) -> Output {
    kl(home).args(args).stdin(Stdio::null()).output().unwrap()
}

/// Write `text` as the default config file of `home`.
fn write_default(home: &Path, text: &str) -> PathBuf {
    let dir = home.join(".kluis");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn missing_default_file_is_created_with_defaults() {
    let home = tempdir();
    let path = home.path().join(".kluis/config.toml");
    assert!(!path.exists());

    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("[generic]\nmodel = \"deepseek-chat\""),
        "{text}"
    );
    assert!(text.contains("deny = []"), "{text}");
    assert!(text.contains("notify = false"), "{text}");
    assert!(!text.contains(".pi"), "{text}");

    // Existing file is not overwritten on the next run.
    std::fs::write(&path, "[sandbox]\nenv = [\"CC\"]\n").unwrap();
    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "[sandbox]\nenv = [\"CC\"]\n"
    );
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
fn web_hosts_are_accepted_and_validated() {
    let home = tempdir();
    write_default(home.path(), "[sandbox]\nweb_hosts = [\"docs.rs\"]\n");
    let out = run_cli(home.path(), &["--network", "--web-host", "localhost"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    // Listed hosts offer web_fetch without network access too, so they are
    // checked then.
    write_default(
        home.path(),
        "[sandbox]\nweb_hosts = [\"https://docs.rs\"]\n",
    );
    let out = run_cli(home.path(), &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("https://docs.rs is not a host name"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn hosts_are_accepted_and_validated() {
    let home = tempdir();
    // The proxy starts with the sandbox.
    write_default(home.path(), "[sandbox]\nhosts = [\"crates.io\"]\n");
    let out = run_cli(home.path(), &["--allow-host", "github.com"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // It needs a sandbox without network, which ports open.
    let out = run_cli(home.path(), &["--allow-connect", "8080"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("--allow-host cannot be used with --allow-connect"),
        "{}",
        stderr(&out)
    );

    write_default(home.path(), "[sandbox]\nhosts = [\"crates.io:443\"]\n");
    let out = run_cli(home.path(), &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("crates.io:443 is not a host name"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn file_in_current_folder_works() {
    // Shown read-only on top of the writable current folder.
    let home = tempdir();
    let cwd = tempdir();
    let sub = cwd.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("config.toml"), "[sandbox]\nenv = [\"CC\"]\n").unwrap();

    let out = kl(home.path())
        .args(["--config", "sub/config.toml"])
        .current_dir(cwd.path())
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
fn no_sandbox_uses_given_config() {
    let home = tempdir();
    let missing = home.path().join("explicit.toml");
    let out = kl(home.path())
        .args(["--no-sandbox", "--config", missing.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("explicit.toml"), "{}", stderr(&out));
}

#[test]
fn deny_accepts_existing_and_missing_paths() {
    let home = tempdir();
    let ssh = home.path().join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    write_default(
        home.path(),
        "[sandbox]\ndeny = [\"~/.ssh\", \"~/.gnupg\"]\n",
    );
    let out = run_cli(home.path(), &["--deny-read", "/no/such/secret"]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn deny_rejects_relative_paths() {
    let home = tempdir();
    write_default(home.path(), "[sandbox]\ndeny = [\".ssh\"]\n");
    let out = run_cli(home.path(), &["--no-sandbox"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(".ssh: not an absolute path"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn max_processes_is_accepted_and_validated() {
    let home = tempdir();
    write_default(home.path(), "[sandbox]\nmax_processes = 512\n");
    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = run_cli(home.path(), &["--max-processes", "256"]);
    assert!(out.status.success(), "{}", stderr(&out));

    write_default(home.path(), "[sandbox]\nmax_processes = 0\n");
    let out = run_cli(home.path(), &["--no-sandbox"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("max_processes: must be at least 1"),
        "{}",
        stderr(&out)
    );

    let out = run_cli(home.path(), &["--no-sandbox", "--max-processes", "0"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--max-processes"), "{}", stderr(&out));
}

#[test]
fn tmp_size_is_accepted_and_validated() {
    let home = tempdir();
    for text in [
        "[sandbox]\ntmp_size = \"64M\"\n",
        "[sandbox]\ntmp_size = 67108864\n",
    ] {
        write_default(home.path(), text);
        let out = run_cli(home.path(), &[]);
        assert!(out.status.success(), "{text}: {}", stderr(&out));
    }

    let out = run_cli(home.path(), &["--tmp-size", "32M"]);
    assert!(out.status.success(), "{}", stderr(&out));

    for bad in [
        "[sandbox]\ntmp_size = 0\n",
        "[sandbox]\ntmp_size = \"0M\"\n",
        "[sandbox]\ntmp_size = \"10X\"\n",
        "[sandbox]\ntmp_size = \"999999999999999G\"\n",
    ] {
        write_default(home.path(), bad);
        let out = run_cli(home.path(), &["--no-sandbox"]);
        assert!(!out.status.success(), "{bad}");
    }

    write_default(home.path(), "");
    let out = run_cli(home.path(), &["--no-sandbox", "--tmp-size", "0"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--tmp-size"), "{}", stderr(&out));
}

#[test]
fn max_memory_is_accepted_and_validated() {
    let home = tempdir();
    for text in [
        "[sandbox]\nmax_memory = \"512M\"\n",
        "[sandbox]\nmax_memory = 536870912\n",
    ] {
        write_default(home.path(), text);
        let out = run_cli(home.path(), &[]);
        assert!(out.status.success(), "{text}: {}", stderr(&out));
    }

    let out = run_cli(home.path(), &["--max-memory", "1G"]);
    assert!(out.status.success(), "{}", stderr(&out));

    for bad in [
        "[sandbox]\nmax_memory = 0\n",
        "[sandbox]\nmax_memory = \"0G\"\n",
        "[sandbox]\nmax_memory = \"10X\"\n",
    ] {
        write_default(home.path(), bad);
        let out = run_cli(home.path(), &["--no-sandbox"]);
        assert!(!out.status.success(), "{bad}");
    }

    write_default(home.path(), "");
    let out = run_cli(home.path(), &["--no-sandbox", "--max-memory", "0"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--max-memory"), "{}", stderr(&out));
}

#[test]
fn model_and_thinking_defaults_are_loaded_and_overridden() {
    let home = tempdir();
    write_default(
        home.path(),
        "[generic]\nmodel = \"claude-sonnet-4-5\"\nthinking = \"medium\"\n",
    );
    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("claude-sonnet-4-5 (medium)"),
        "{}",
        stdout(&out)
    );

    // CLI flags override config defaults.
    let out = run_cli(home.path(), &["--model", "gpt-4o", "--thinking", "high"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("gpt-4o (high)"), "{}", stdout(&out));

    // Invalid config values and invalid CLI thinking levels are rejected.
    write_default(home.path(), "[generic]\nmodel = \"   \"\n");
    let out = run_cli(home.path(), &["--no-sandbox"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("model"), "{}", stderr(&out));

    write_default(home.path(), "[generic]\nthinking = \"turbo\"\n");
    let out = run_cli(home.path(), &["--no-sandbox"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("turbo"), "{}", stderr(&out));

    // Top-level settings are no longer accepted.
    write_default(home.path(), "model = \"gpt-4o\"\n");
    let out = run_cli(home.path(), &["--no-sandbox"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("model"), "{}", stderr(&out));

    write_default(home.path(), "");
    let out = run_cli(home.path(), &["--no-sandbox", "--thinking", "turbo"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("invalid thinking effort"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn searxng_url_is_accepted_and_validated() {
    let home = tempdir();
    write_default(
        home.path(),
        "[generic]\nsearxng_url = \"https://searx.example.org\"\n",
    );
    let out = run_cli(home.path(), &["--network"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // Checked only when the tool is offered: with network access.
    write_default(home.path(), "[generic]\nsearxng_url = \"nope\"\n");
    let out = run_cli(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run_cli(home.path(), &["--network"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nope"), "{}", stderr(&out));

    // The flag wins over the file.
    let out = run_cli(
        home.path(),
        &["--network", "--searxng-url", "http://localhost:8888"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
}
