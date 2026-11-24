// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tests of `kl --print` and `kl --output-format` against a local mock server.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Output, Stdio};
use std::thread::{self, JoinHandle};

use common::{delta, kill, kl, read_request, stderr, tempdir, write_sse};
use serde_json::{json, Value};
use tempfile::TempDir;

/// Start a server that answers one request with `status`, and with a
/// streamed "done" when it is 200. It returns the user message it got.
fn start_mock_llm(status: u16) -> (u16, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&stream);
        let user = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default();
        if status == 200 {
            let usage = json!({ "prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15 });
            let mut chunks = [
                delta(json!({ "content": "done" }), None),
                delta(json!({}), Some("stop")),
            ];
            for chunk in &mut chunks {
                chunk["usage"] = usage.clone();
            }
            write_sse(stream, &chunks);
        } else {
            let body = r#"{"error":{"message":"bad key"}}"#;
            let reply = format!(
                "HTTP/1.1 {status} Error\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
        user
    });
    (port, handle)
}

/// Run `kl` with `args`, `stdin` as its standard input, against a server
/// answering with `status`; return its output and the server, which gives
/// the user message sent. Joined only after checking the output, so that a
/// `kl` that never connects fails the test instead of hanging it.
fn run_kl(args: &[&str], stdin: &str, status: u16) -> (Output, JoinHandle<String>) {
    run_kl_in(&tempdir(), args, stdin, status)
}

/// [`run_kl`] with `home` as `$HOME`.
fn run_kl_in(
    home: &TempDir,
    args: &[&str],
    stdin: &str,
    status: u16,
) -> (Output, JoinHandle<String>) {
    let (port, server) = start_mock_llm(status);
    let cwd = tempdir();
    let mut child = kl(home.path())
        .args(["--model", "genai_1::test-model", "--no-session"])
        .args(args)
        .current_dir(cwd.path())
        .env("GENAI_1_ENDPOINT", format!("http://127.0.0.1:{port}/v1/"))
        .env("GENAI_1_API_KEY", "test-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (out, server)
}

#[test]
fn print_appends_standard_input_to_the_prompt() {
    let (out, server) = run_kl(&["-p", "review this"], "some diff\n", 200);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(server.join().unwrap(), "review this\n\nsome diff\n");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "done");
}

#[test]
fn json_prints_the_result_as_one_object() {
    let (out, server) = run_kl(&["--output-format", "json"], "hello", 200);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(server.join().unwrap(), "hello");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["result"], "done");
    assert_eq!(result["usage"]["output"], 3);
    assert_eq!(result["session"], Value::Null);
}

#[test]
fn json_reports_a_failure_with_its_exit_code() {
    let (out, _) = run_kl(&["--output-format", "json", "hello"], "", 401);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        result["error"].as_str().unwrap().contains("401"),
        "{result}"
    );
}

#[test]
fn hints_at_newer_bundled_skills() {
    let home = tempdir();
    let skills = home.path().join(".kluis/skills");
    let install = kl(home.path()).arg("--install-skills").output().unwrap();
    assert!(install.status.success(), "{install:?}");
    let (out, _) = run_kl_in(&home, &["-p", "hello"], "", 200);
    assert!(!stderr(&out).contains("--install-skills"), "{out:?}");

    // As if the skills came from an older kl.
    std::fs::write(skills.join(".bundled"), "").unwrap();
    let (out, _) = run_kl_in(&home, &["-p", "hello"], "", 200);
    assert!(
        stderr(&out).contains("newer versions of the skills mentor, study come with kl"),
        "{out:?}"
    );
}

#[test]
fn line_mode_fails_when_a_prompt_failed() {
    let (out, _) = run_kl(&[], "hello\n", 401);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
}

fn json_lines(stdout: &[u8]) -> Vec<Value> {
    stdout
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

#[test]
fn stream_prints_one_line_per_event() {
    let (out, server) = run_kl(&["--output-format", "stream", "hello"], "", 200);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(server.join().unwrap(), "hello");
    let lines = json_lines(&out.stdout);
    let types: Vec<_> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["text", "usage", "result"], "{lines:?}");
    assert_eq!(lines[0]["text"], "done");
    assert_eq!(lines[1]["output"], 3);
    assert_eq!(lines[2]["result"], "done");
    assert_eq!(lines[2]["usage"]["output"], 3);
    assert_eq!(lines[2]["session"], Value::Null);
}

#[test]
fn stream_ends_with_the_error() {
    let (out, _) = run_kl(&["--output-format", "stream", "hello"], "", 401);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let lines = json_lines(&out.stdout);
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "error");
    assert!(last["error"].as_str().unwrap().contains("401"), "{last}");
}

#[test]
fn signal_ends_the_prompts_read_from_standard_input() {
    let home = tempdir();
    let mut child = kl(home.path())
        .arg("--no-sandbox")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Printed once it waits for a prompt.
    // Kept open: nothing but the signal ends the prompts.
    let _stdin = child.stdin.take();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut greeting = String::new();
    stdout.read_line(&mut greeting).unwrap();
    assert!(greeting.contains("Ctrl-D to quit"), "{greeting}");

    kill(&child, libc::SIGTERM);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
}
