// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tests running the real `kl` binary against a local OpenAI-compatible
//! mock server to verify what commands executed by the `bash` tool can see
//! and do inside the sandbox.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};

use serde_json::{json, Value};
use tempfile::TempDir;

fn tempdir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

struct RunResult {
    system: String,
    messages: Vec<Value>,
    output: String,
}

/// Start a mock OpenAI SSE server that answers the first request with a
/// `bash` tool call built from its listening port, captures the first
/// request's system prompt, messages and the second request's tool output, and
/// replies `"done"`.
fn start_mock_llm(make_cmd: impl FnOnce(u16) -> String) -> (u16, JoinHandle<RunResult>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bash_cmd = make_cmd(port);

    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let req1 = read_request(&stream);
        let messages = req1["messages"].as_array().cloned().unwrap_or_default();
        let system = messages
            .iter()
            .find(|m| m["role"] == "system")
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default()
            .to_owned();
        let call = json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": json!({ "command": bash_cmd }).to_string(),
            }
        });
        write_sse(
            stream,
            &[
                delta(json!({ "tool_calls": [call] }), None),
                delta(json!({}), Some("tool_calls")),
            ],
        );

        let (stream, _) = listener.accept().unwrap();
        let req2 = read_request(&stream);
        let content = req2["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .and_then(|m| m["content"].as_str())
            .unwrap();
        let output = serde_json::from_str::<Value>(content).unwrap()["output"]
            .as_str()
            .unwrap()
            .to_owned();
        write_sse(
            stream,
            &[
                delta(json!({ "content": "done" }), None),
                delta(json!({}), Some("stop")),
            ],
        );
        RunResult {
            system,
            messages,
            output,
        }
    });

    (port, handle)
}

/// Run `kl` in `cwd` with a single `bash` tool call and return the captured
/// system prompt and command output.
fn run_bash_in(
    cwd: &Path,
    home: &Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
    make_cmd: impl FnOnce(u16) -> String,
) -> RunResult {
    let (llm_port, server) = start_mock_llm(make_cmd);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kl"));
    cmd.args([
        "--model",
        "genai_1::test-model",
        "--allow-connect",
        &llm_port.to_string(),
    ])
    .args(extra_args)
    .arg("run")
    .current_dir(cwd)
    .env("HOME", home)
    .env(
        "GENAI_1_ENDPOINT",
        format!("http://127.0.0.1:{llm_port}/v1/"),
    )
    .env("GENAI_1_API_KEY", "secret-genai-key-12345")
    .env_remove("HARNESS_CONFIG")
    .env_remove("HARNESS_AGENTS")
    .env_remove("HARNESS_SANDBOXED")
    .stdin(Stdio::null());
    for &(k, v) in extra_env {
        cmd.env(k, v);
    }

    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    server.join().unwrap()
}

/// Run `kl` with a single `bash` tool call in a fresh folder and return the
/// command output.
fn run_bash(
    home: &Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
    make_cmd: impl FnOnce(u16) -> String,
) -> String {
    let cwd = tempdir();
    run_bash_in(cwd.path(), home, extra_args, extra_env, make_cmd).output
}

fn read_request(stream: &TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn write_sse(mut stream: TcpStream, chunks: &[String]) {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).unwrap();
    for chunk in chunks {
        stream
            .write_all(format!("data: {chunk}\n\n").as_bytes())
            .unwrap();
    }
    stream.write_all(b"data: [DONE]\n\n").unwrap();
    stream.flush().unwrap();
}

fn delta(delta: Value, finish_reason: Option<&str>) -> String {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
    })
    .to_string()
}

fn write_config(home: &Path, text: &str) {
    let dir = home.join(".kluis");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

#[test]
fn commands_get_no_network() {
    let home = tempdir();
    let other_server = TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other_server.local_addr().unwrap().port();

    let output = run_bash(home.path(), &[], &[], |llm_port| {
        format!(
            "echo LLM=$( (exec 3<>/dev/tcp/127.0.0.1/{llm_port}) 2>&1 || true ); \
             echo OTHER=$( (exec 3<>/dev/tcp/127.0.0.1/{other_port}) 2>&1 || true )"
        )
    });

    // Even the port allowed for the harness is unreachable to commands.
    let llm = output.lines().find(|l| l.starts_with("LLM=")).unwrap();
    assert!(llm.contains("unreachable"), "{llm}");
    let other = output.lines().find(|l| l.starts_with("OTHER=")).unwrap();
    assert!(
        other.contains("Permission denied") || other.contains("unreachable"),
        "{other}"
    );
    drop(other_server);
}

#[test]
fn commands_get_no_api_keys() {
    let home = tempdir();
    let output = run_bash(
        home.path(),
        &["--pass-env", "ALLOWED_VAR"],
        &[
            ("DEEPSEEK_API_KEY", "secret-deepseek-key-67890"),
            ("ALLOWED_VAR", "visible-value"),
        ],
        |_| "env; cat /proc/1/environ 2>&1 || true".to_string(),
    );

    assert!(output.contains("ALLOWED_VAR=visible-value"), "{output}");
    assert!(!output.contains("secret-genai-key-12345"), "{output}");
    assert!(!output.contains("secret-deepseek-key-67890"), "{output}");
    assert!(!output.contains("GENAI_1_API_KEY"), "{output}");
    assert!(!output.contains("DEEPSEEK_API_KEY"), "{output}");
    assert!(
        output.contains("/proc/1/environ: Permission denied"),
        "{output}"
    );
}

#[test]
fn default_config_hides_ssh_and_stays_read_only() {
    let home = tempdir();
    let ssh = home.path().join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    std::fs::write(ssh.join("id_ed25519"), "ssh-private-secret").unwrap();

    let output = run_bash(home.path(), &[], &[], |_| {
        format!(
            "echo SSH=$(cat {}/id_ed25519 2>&1 || true); \
             echo CFG=$( (echo evil >> {}/.kluis/config.toml) 2>&1 || true )",
            ssh.display(),
            home.path().display()
        )
    });

    assert!(!output.contains("ssh-private-secret"), "{output}");
    let ssh_line = output.lines().find(|l| l.starts_with("SSH=")).unwrap();
    assert!(ssh_line.contains("Permission denied"), "{ssh_line}");
    let cfg_line = output.lines().find(|l| l.starts_with("CFG=")).unwrap();
    assert!(cfg_line.contains("Read-only file system"), "{cfg_line}");
}

#[test]
fn config_env_reaches_commands() {
    let home = tempdir();
    write_config(home.path(), "[sandbox]\nenv = [\"MY_BUILD_FLAG\"]\n");

    let output = run_bash(home.path(), &[], &[("MY_BUILD_FLAG", "opt-o2")], |_| {
        "printf 'flag=%s\\n' \"$MY_BUILD_FLAG\"".to_string()
    });
    assert!(output.contains("flag=opt-o2"), "{output}");
}

#[test]
fn config_max_memory_caps_commands() {
    let home = tempdir();
    write_config(home.path(), "[sandbox]\nmax_memory = \"256M\"\n");

    let output = run_bash(home.path(), &[], &[], |_| "ulimit -v".to_string());
    assert_eq!(output.trim(), (256 << 10).to_string());
}

#[test]
fn agents_md_loaded_and_protected_in_sandbox() {
    let home = tempdir();
    let kluis_dir = home.path().join(".kluis");
    std::fs::create_dir_all(&kluis_dir).unwrap();
    let global_agents = kluis_dir.join("AGENTS.md");
    std::fs::write(&global_agents, "global-agent-rule\n").unwrap();

    let repo = tempdir();
    let sub = repo.path().join("sub");
    std::fs::create_dir_all(repo.path().join(".git")).unwrap();
    std::fs::create_dir_all(&sub).unwrap();
    let repo_agents = repo.path().join("AGENTS.md");
    std::fs::write(&repo_agents, "repo-agent-rule\n").unwrap();
    std::fs::write(sub.join("AGENTS.md"), "subdir-agent-rule\n").unwrap();

    let result = run_bash_in(&sub, home.path(), &[], &[], |_| {
        format!(
            "echo GLOBAL=$( (echo evil >> {}) 2>&1 || true ); \
             echo REPO=$( (echo evil >> {}) 2>&1 || true )",
            global_agents.display(),
            repo_agents.display(),
        )
    });

    let global_pos = result
        .system
        .find("global-agent-rule")
        .expect("global AGENTS.md");
    let repo_pos = result
        .system
        .find("repo-agent-rule")
        .expect("repo AGENTS.md");
    let sub_pos = result
        .system
        .find("subdir-agent-rule")
        .expect("subdir AGENTS.md");
    assert!(
        global_pos < repo_pos && repo_pos < sub_pos,
        "{}",
        result.system
    );
    assert!(
        result.system.contains(&sub.display().to_string()),
        "{}",
        result.system
    );
    assert!(result.system.contains("OS: linux"), "{}", result.system);

    let global_line = result
        .output
        .lines()
        .find(|l| l.starts_with("GLOBAL="))
        .unwrap();
    assert!(
        global_line.contains("Read-only file system"),
        "{global_line}"
    );
    let repo_line = result
        .output
        .lines()
        .find(|l| l.starts_with("REPO="))
        .unwrap();
    assert!(repo_line.contains("Read-only file system"), "{repo_line}");
}

#[test]
fn session_persists_continues_and_isolates_projects_in_sandbox() {
    let home = tempdir();
    let proj_a = tempdir();
    let proj_b = tempdir();

    // Seed a session in another project to confirm it stays invisible to proj_a.
    let other_store =
        harness_core::SessionStore::new(home.path().join(".kluis").join("sessions"), proj_b.path());
    let mut other_writer = other_store.create("other-model");
    other_writer
        .append_messages(&[genai::chat::ChatMessage::user("secret-other-project")])
        .unwrap();

    // First run in proj_a saves a session file (and cannot see proj_b's session).
    let first = run_bash_in(proj_a.path(), home.path(), &[], &[], |_| {
        format!("cat {} 2>&1 || true", other_writer.path().display())
    });
    assert!(
        !first.output.contains("secret-other-project"),
        "{}",
        first.output
    );

    // Second run with `-c` restores the prior turn's history and sends it to the LLM.
    let second = run_bash_in(proj_a.path(), home.path(), &["-c"], &[], |_| {
        "echo ok".to_string()
    });
    let user_msgs: Vec<&str> = second
        .messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(user_msgs, vec!["run", "run"], "{:?}", second.messages);
    let assistant_msgs: Vec<&str> = second
        .messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(
        assistant_msgs.contains(&"done"),
        "expected prior assistant reply in continued session: {:?}",
        second.messages
    );

    // `--no-session` creates no session directory in a fresh project.
    let proj_c = tempdir();
    let _ = run_bash_in(proj_c.path(), home.path(), &["--no-session"], &[], |_| {
        "echo ephemeral".to_string()
    });
    let store_c =
        harness_core::SessionStore::new(home.path().join(".kluis").join("sessions"), proj_c.path());
    assert!(!store_c.project_dir().exists());

    // `-c` in a folder with no session fails with a clear error.
    let out = Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(["-c"])
        .current_dir(proj_c.path())
        .env("HOME", home.path())
        .env_remove("HARNESS_CONFIG")
        .env_remove("HARNESS_AGENTS")
        .env_remove("HARNESS_SANDBOXED")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no session to continue"), "{stderr}");

    // `--resume <prefix>` selects a specific earlier session inside the sandbox.
    let store_a =
        harness_core::SessionStore::new(home.path().join(".kluis").join("sessions"), proj_a.path());
    let mut old_writer = store_a.create("model-old");
    old_writer
        .append_messages(&[
            genai::chat::ChatMessage::user("remember-target-session"),
            genai::chat::ChatMessage::assistant("noted-target"),
        ])
        .unwrap();
    let target_prefix = old_writer.id()[..20].to_owned();

    let mut newer_writer = store_a.create("model-new");
    newer_writer
        .append_messages(&[genai::chat::ChatMessage::user("different-newer-session")])
        .unwrap();

    let resumed = run_bash_in(
        proj_a.path(),
        home.path(),
        &["--resume", &target_prefix],
        &[],
        |_| "echo resumed".to_string(),
    );
    let resumed_users: Vec<&str> = resumed
        .messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(
        resumed_users,
        vec!["remember-target-session", "run"],
        "{:?}",
        resumed.messages
    );

    // Bare `--resume` without a TTY fails with a clear message.
    let no_tty = Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(["--resume"])
        .current_dir(proj_a.path())
        .env("HOME", home.path())
        .env_remove("HARNESS_CONFIG")
        .env_remove("HARNESS_AGENTS")
        .env_remove("HARNESS_SANDBOXED")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!no_tty.status.success());
    let stderr = String::from_utf8_lossy(&no_tty.stderr);
    assert!(
        stderr.contains("requires an interactive terminal"),
        "{stderr}"
    );
}
