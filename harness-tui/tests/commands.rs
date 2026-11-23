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
    /// What `kl` printed to its standard error.
    stderr: String,
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
            stderr: String::new(),
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
    cmd.args(["--model", "genai_1::test-model"])
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
    let mut result = server.join().unwrap();
    result.stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    result
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
    read_request_with_headers(stream).1
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
}

#[test]
fn default_config_hides_ssh_and_kluis_dir() {
    let home = tempdir();
    let ssh = home.path().join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    std::fs::write(ssh.join("id_ed25519"), "ssh-private-secret").unwrap();

    let output = run_bash(home.path(), &[], &[], |_| {
        format!(
            "echo SSH=$(cat {}/id_ed25519 2>&1 || true); \
             echo CFG_R=$(cat {1}/.kluis/config.toml 2>&1 || true); \
             echo CFG_W=$( (echo evil >> {1}/.kluis/config.toml) 2>&1 || true )",
            ssh.display(),
            home.path().display()
        )
    });

    assert!(!output.contains("ssh-private-secret"), "{output}");
    let ssh_line = output.lines().find(|l| l.starts_with("SSH=")).unwrap();
    assert!(ssh_line.contains("Permission denied"), "{ssh_line}");
    let cfg_r = output.lines().find(|l| l.starts_with("CFG_R=")).unwrap();
    assert!(cfg_r.contains("Permission denied"), "{cfg_r}");
    let cfg_w = output.lines().find(|l| l.starts_with("CFG_W=")).unwrap();
    assert!(cfg_w.contains("Permission denied"), "{cfg_w}");
}

#[test]
fn credentials_in_home_stay_hidden() {
    // Whatever the config hides besides: the home folder is shown.
    let home = tempdir();
    write_config(home.path(), "[sandbox]\ndeny = [\"/no/such/secret\"]\n");
    let files = [
        ".ssh/id_ed25519",
        ".config/gh/hosts.yml",
        ".aws/credentials",
        ".bash_history",
        ".claude.json",
        ".codex/auth.json",
        ".config/tool.toml",
    ];
    for file in files {
        let path = home.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("CONTENT-OF {file}\n")).unwrap();
    }

    let output = run_bash(home.path(), &[], &[], |_| {
        format!(
            r#"cd {}
               for f in {}; do
                 cat "$f" 2>/dev/null || echo "HIDDEN $f"
               done"#,
            home.path().display(),
            files.join(" ")
        )
    });
    for file in &files[..files.len() - 1] {
        assert!(output.contains(&format!("HIDDEN {file}")), "{output}");
    }
    assert!(output.contains("CONTENT-OF .config/tool.toml"), "{output}");
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
    assert!(global_line.contains("Permission denied"), "{global_line}");
    let repo_line = result
        .output
        .lines()
        .find(|l| l.starts_with("REPO="))
        .unwrap();
    assert!(
        repo_line.contains("No such file") || repo_line.contains("Read-only file system"),
        "{repo_line}"
    );
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

#[test]
fn auth_toml_authenticates_host_while_hidden_in_sandbox() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir();
    let cwd = tempdir();
    let kluis = home.path().join(".kluis");
    std::fs::create_dir_all(&kluis).unwrap();
    let auth_path = kluis.join("auth.toml");

    // Open permissions (0644) must be rejected before starting.
    std::fs::write(&auth_path, "[mistral]\napi_key = \"sk-open\"\n").unwrap();
    std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let bad = Command::new(env!("CARGO_BIN_EXE_kl"))
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env_remove("HARNESS_CONFIG")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!bad.status.success());
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(stderr.contains("chmod 600"), "{stderr}");

    // Start a mock OpenAI-compatible server that captures the Authorization header
    // and runs a bash command trying to read `~/.kluis/auth.toml`.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let auth_path_str = auth_path.display().to_string();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let (headers, _body) = read_request_with_headers(&stream);
        let call = json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": json!({
                    "command": format!(
                        "echo AUTH=$(cat {auth_path_str} 2>&1 || true); env; cat /proc/1/environ 2>&1 || true"
                    )
                })
                .to_string(),
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
        (headers, output)
    });

    std::fs::write(
        &auth_path,
        format!(
            "[mistral]\napi_key = \"secret-from-auth-toml\"\nendpoint = \"http://127.0.0.1:{port}/v1\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(["--model", "mistral::test-model", "run"])
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env_remove("HARNESS_CONFIG")
        .env_remove("MISTRAL_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let (headers, bash_output) = server.join().unwrap();
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer secret-from-auth-toml"),
        "{headers}"
    );
    assert!(
        !bash_output.contains("secret-from-auth-toml"),
        "auth.toml leaked into sandbox: {bash_output}"
    );
    let auth_line = bash_output
        .lines()
        .find(|l| l.starts_with("AUTH="))
        .unwrap();
    assert!(auth_line.contains("Permission denied"), "{auth_line}");
}

fn read_request_with_headers(stream: &TcpStream) -> (String, Value) {
    let mut reader = BufReader::new(stream);
    let mut headers = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        headers.push_str(trimmed);
        headers.push('\n');
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    (headers, serde_json::from_slice(&body).unwrap())
}

/// Set when this test program runs inside the sandbox as the socket
/// probe of the model's command.
const SOCKET_PROBE: &str = "KL_TEST_SOCKET_PROBE";

/// Run inside the sandbox by the model's command: try the host's Unix
/// socket at the path in $SOCK, then a socket pair, and print the results.
#[test]
fn socket_probe() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    if std::env::var_os(SOCKET_PROBE).is_none() {
        return;
    }
    match UnixStream::connect(std::env::var("SOCK").unwrap()) {
        Ok(mut stream) => {
            stream.write_all(b"escaped").unwrap();
            println!("UNIX=connected");
        }
        Err(e) => println!("UNIX={:?}", e.kind()),
    }
    let (mut a, mut b) = UnixStream::pair().unwrap();
    a.write_all(b"x").unwrap();
    let mut buf = [0];
    b.read_exact(&mut buf).unwrap();
    println!("PAIR={}", buf[0] as char);
}

/// Run the socket probe through `kl` with `extra_args`, against a socket
/// listening on the host in the readable home folder, and return its
/// output and whether the host received anything.
fn probe_host_socket(extra_args: &[&str]) -> (String, bool) {
    let home = tempdir();
    let sock = home.path().join("app.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    listener.set_nonblocking(true).unwrap();
    let probe = std::env::current_exe().unwrap();
    let probe = probe.to_str().unwrap();
    let sock = sock.to_str().unwrap();

    let mut args = vec!["--pass-env", "SOCK", "--pass-env", SOCKET_PROBE];
    args.extend(["--allow-read", probe]);
    args.extend(extra_args);
    let env = [("SOCK", sock), (SOCKET_PROBE, "1")];
    let output = run_bash(home.path(), &args, &env, |_| {
        format!("{probe} --exact socket_probe --nocapture")
    });
    let received = listener.accept().is_ok();
    (output, received)
}

#[test]
fn commands_cannot_reach_host_unix_sockets() {
    let (output, received) = probe_host_socket(&[]);
    let unix = output.lines().find(|l| l.starts_with("UNIX=")).unwrap();
    assert_eq!(unix, "UNIX=PermissionDenied", "{output}");
    assert!(!received, "the host socket got a connection");
    // A socket pair only reaches itself, so it keeps working.
    assert!(output.contains("PAIR=x"), "{output}");
}

#[test]
fn network_allows_host_unix_sockets() {
    // With any network allowed there is nothing to keep away.
    let (output, received) = probe_host_socket(&["--network"]);
    assert!(output.contains("UNIX=connected"), "{output}");
    assert!(received);
}

#[test]
fn commands_cannot_create_user_namespaces() {
    // Kernel exploits mostly go through them, network or not.
    let home = tempdir();
    let output = run_bash(home.path(), &["--network"], &[], |_| {
        "unshare --user true 2>&1; echo EXIT=$?".into()
    });
    assert!(output.contains("Operation not permitted"), "{output}");
    assert!(output.contains("EXIT=1"), "{output}");
}

#[test]
fn repositories_commands_create_are_disabled() {
    // Git on the host would run what their config names.
    let home = tempdir();
    let cwd = tempdir();
    run_bash_in(cwd.path(), home.path(), &[], &[], |_| {
        "mkdir -p .git sub/.git && echo '[core]' > sub/.git/config".into()
    });

    for dir in [cwd.path().to_path_buf(), cwd.path().join("sub")] {
        assert!(!dir.join(".git").exists(), "{}", dir.display());
        assert!(dir.join(".git.kl-disabled").is_dir(), "{}", dir.display());
    }
}

#[test]
fn commands_cannot_reach_the_worker() {
    // Its pipes carry the tool results to the host. It is the process
    // whose first argument is `--tool-executor`, below an init forked
    // from it.
    let home = tempdir();
    let output = run_bash(home.path(), &[], &[], |_| {
        r#"for p in /proc/[0-9]*; do
             [ "$(tr '\0' '\n' < $p/cmdline | sed -n 2p)" = --tool-executor ] || continue
             echo "WORKER=${p#/proc/}"
             head -c 1 $p/environ >/dev/null 2>&1 && echo "ENV=readable"
             (exec 3>>$p/fd/1) 2>/dev/null && echo "PIPE=writable"
           done"#
            .into()
    });
    assert!(output.contains("WORKER=1"), "{output}");
    assert!(output.contains("WORKER=2"), "{output}");
    assert!(!output.contains("readable"), "{output}");
    assert!(!output.contains("writable"), "{output}");
}

#[test]
fn allowed_ports_do_not_open_udp() {
    // Landlock only limits TCP: UDP would reach any host.
    let home = tempdir();
    let output = run_bash(home.path(), &["--allow-connect", "9"], &[], |_| {
        "(exec 3<>/dev/udp/127.0.0.1/53) 2>&1".into()
    });
    assert!(output.contains("Permission denied"), "{output}");
}

#[test]
fn programs_left_running_are_stopped_after_the_prompt() {
    // Before the git check, which one could beat by recreating a repository.
    let home = tempdir();
    let cwd = tempdir();
    run_bash_in(cwd.path(), home.path(), &[], &[], |_| {
        r#"(setsid sh -c 'while :; do mkdir -p sub/.git; done' >/dev/null 2>&1 &)
           while [ ! -d sub/.git ]; do sleep 0.01; done"#
            .into()
    });

    let sub = cwd.path().join("sub");
    assert!(sub.join(".git.kl-disabled").is_dir());
    assert!(!sub.join(".git").exists());
}

#[test]
fn programs_left_behind_are_reaped() {
    // The kernel hands them to the container's init.
    let home = tempdir();
    let output = run_bash(home.path(), &[], &[], |_| {
        r#"(setsid sleep 0.1 &)
           sleep 0.5
           grep -l '^State:.Z' /proc/[0-9]*/status | wc -l"#
            .into()
    });
    assert_eq!(output.trim(), "0", "{output}");
}

/// What `kl` warns about git with `config` as the only git configuration.
fn git_warning(config: &str) -> String {
    git_warning_in(tempdir().path(), config)
}

/// Like [`git_warning`], run in `cwd`.
fn git_warning_in(cwd: &Path, config: &str) -> String {
    run_bash_with_git(cwd, config, "true").stderr
}

/// Run `cmd` with `kl` in `cwd`, with `config` as the only git configuration.
fn run_bash_with_git(cwd: &Path, config: &str, cmd: &str) -> RunResult {
    let home = tempdir();
    let gitconfig = home.path().join("gitconfig");
    std::fs::write(&gitconfig, config).unwrap();
    // `CARGO_TARGET_TMPDIR` lives inside this repository's `target/`, so stop
    // git from climbing past it and discovering the outer `.git`.
    let ceiling = std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let env = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", gitconfig.to_str().unwrap()),
        ("GIT_CEILING_DIRECTORIES", ceiling.to_str().unwrap()),
    ];
    run_bash_in(cwd, home.path(), &[], &env, |_| cmd.into())
}

#[test]
fn warns_when_git_uses_repositories_under_any_name() {
    // Git would take a folder of commands for a repository.
    let stderr = git_warning("");
    assert!(stderr.contains("kl --setup-git"), "{stderr}");
}

#[test]
fn no_git_warning_when_only_dot_git_is_used() {
    let stderr = git_warning("[safe]\n\tbareRepository = explicit\n");
    assert!(!stderr.contains("kl --setup-git"), "{stderr}");
}

/// A folder holding a new git repository.
fn repository() -> TempDir {
    let dir = tempdir();
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    dir
}

#[test]
fn warns_when_git_runs_hooks_of_the_project() {
    let repo = repository();
    let stderr = git_warning_in(repo.path(), "");
    assert!(stderr.contains("core.hooksPath /dev/null"), "{stderr}");
}

/// Set `core.hooksPath` of the repository in `repo` to `path`.
fn set_hooks_path(repo: &Path, path: &str) {
    let status = Command::new("git")
        .args(["config", "core.hooksPath", path])
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn hooks_folder_of_the_project_stays_read_only() {
    // Git on the host would run what commands write there.
    let repo = repository();
    set_hooks_path(repo.path(), ".husky/_");
    let hooks = repo.path().join(".husky/_");
    std::fs::create_dir_all(&hooks).unwrap();
    let result = run_bash_with_git(
        repo.path(),
        "",
        "echo x > .husky/_/pre-commit; echo x > .husky/pre-commit; echo done",
    );
    assert!(result.output.contains("Read-only"), "{}", result.output);
    assert!(!hooks.join("pre-commit").exists());
    // The rest of the project stays writable.
    assert!(repo.path().join(".husky/pre-commit").exists());
}

#[test]
fn hooks_warning_points_to_the_docs_when_the_repository_sets_hooks() {
    // As husky does: the global setting cannot turn these off.
    let repo = repository();
    set_hooks_path(repo.path(), ".husky/_");
    let stderr = git_warning_in(repo.path(), "[core]\n\thooksPath = /dev/null\n");
    assert!(stderr.contains("sets its own hooks"), "{stderr}");
    assert!(stderr.contains("#git-safety"), "{stderr}");
}

#[test]
fn no_hooks_warning_when_hooks_are_off() {
    let repo = repository();
    let stderr = git_warning_in(repo.path(), "[core]\n\thooksPath = /dev/null\n");
    assert!(!stderr.contains("core.hooksPath"), "{stderr}");
}

#[test]
fn no_hooks_warning_outside_a_repository() {
    let stderr = git_warning("");
    assert!(!stderr.contains("core.hooksPath"), "{stderr}");
}

#[test]
fn commands_go_first_when_memory_runs_out() {
    // Before the user's own programs.
    let home = tempdir();
    let output = run_bash(home.path(), &[], &[], |_| {
        "cat /proc/self/oom_score_adj".into()
    });
    assert_eq!(output.trim(), "1000", "{output}");
}

#[test]
fn notices_send_no_escape_sequences() {
    // A command picks the names in them, which could set the clipboard.
    let home = tempdir();
    let cwd = tempdir();
    let result = run_bash_in(cwd.path(), home.path(), &[], &[], |_| {
        r#"name=$(printf 'x\033]52;c;cHduZWQ=\007')
           mkdir -p "$name/.git""#
            .into()
    });
    assert!(result.stderr.contains("kl-disabled"), "{}", result.stderr);
    assert!(
        !result.stderr.contains(['\x1b', '\x07']),
        "{}",
        result.stderr
    );
}
