// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tests of `kl --print` and `kl --json` against a local mock server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::thread::{self, JoinHandle};

use serde_json::{json, Value};
use tempfile::TempDir;

fn tempdir() -> TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

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
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        connection: close\r\n\r\n";
            stream.write_all(head.as_bytes()).unwrap();
            for (delta, finish) in [
                (json!({ "content": "done" }), None),
                (json!({}), Some("stop")),
            ] {
                let chunk = json!({
                    "id": "chatcmpl-test",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "test-model",
                    "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
                    "usage": { "prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15 }
                });
                stream
                    .write_all(format!("data: {chunk}\n\n").as_bytes())
                    .unwrap();
            }
            stream.write_all(b"data: [DONE]\n\n").unwrap();
        } else {
            let body = r#"{"error":{"message":"bad key"}}"#;
            let reply = format!(
                "HTTP/1.1 {status} Error\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
        }
        stream.flush().unwrap();
        user
    });
    (port, handle)
}

fn read_request(stream: &TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut length = 0;
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
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// Run `kl` with `args`, `stdin` as its standard input, against a server
/// answering with `status`; return its output and the server, which gives
/// the user message sent. Joined only after checking the output, so that a
/// `kl` that never connects fails the test instead of hanging it.
fn run_kl(args: &[&str], stdin: &str, status: u16) -> (Output, JoinHandle<String>) {
    let (port, server) = start_mock_llm(status);
    let home = tempdir();
    let cwd = tempdir();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(["--model", "genai_1::test-model", "--no-session"])
        .args(args)
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("GENAI_1_ENDPOINT", format!("http://127.0.0.1:{port}/v1/"))
        .env("GENAI_1_API_KEY", "test-key")
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
    let (out, server) = run_kl(&["--json"], "hello", 200);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(server.join().unwrap(), "hello");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["result"], "done");
    assert_eq!(result["usage"]["output"], 3);
    assert_eq!(result["session"], Value::Null);
}

#[test]
fn json_reports_a_failure_with_its_exit_code() {
    let (out, _) = run_kl(&["--json", "hello"], "", 401);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        result["error"].as_str().unwrap().contains("401"),
        "{result}"
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
fn json_stream_prints_one_line_per_event() {
    let (out, server) = run_kl(&["--json-stream", "hello"], "", 200);

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
fn json_stream_ends_with_the_error() {
    let (out, _) = run_kl(&["--json-stream", "hello"], "", 401);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let lines = json_lines(&out.stdout);
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "error");
    assert!(last["error"].as_str().unwrap().contains("401"), "{last}");
}
