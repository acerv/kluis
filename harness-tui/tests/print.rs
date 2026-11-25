// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tests of `kl --print` and `kl --output-format` against a local mock server.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Output, Stdio};
use std::thread::{self, JoinHandle};

use common::{delta, kill, kl, read_request, stderr, tempdir, write_sse};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The last user message of `request`.
fn last_user(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_default()
}

/// Answer a request with the streamed `text`.
fn write_answer(stream: TcpStream, text: &str) {
    let usage = json!({ "prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15 });
    let mut chunks = [
        delta(json!({ "content": text }), None),
        delta(json!({}), Some("stop")),
    ];
    for chunk in &mut chunks {
        chunk["usage"] = usage.clone();
    }
    write_sse(stream, &chunks);
}

/// Start a server that answers one request with `status`, and with a
/// streamed "done" when it is 200. It returns the user message it got.
fn start_mock_llm(status: u16) -> (u16, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let user = last_user(&read_request(&stream));
        if status == 200 {
            write_answer(stream, "done");
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

/// Start a server that answers a request with each of `answers`. It
/// returns the user messages it got.
fn start_mock_answers(answers: &[&str]) -> (u16, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let answers: Vec<_> = answers.iter().map(|&answer| answer.to_owned()).collect();
    let handle = thread::spawn(move || {
        let mut users = Vec::new();
        for answer in &answers {
            let (stream, _) = listener.accept().unwrap();
            users.push(last_user(&read_request(&stream)));
            write_answer(stream, answer);
        }
        users
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
    let mut all = vec!["--no-session"];
    all.extend(args);
    run_kl_at(home, &tempdir(), &all, stdin, status)
}

/// [`run_kl`] with `home` as `$HOME`, in the folder `cwd`, saving the
/// session unless `args` say otherwise.
fn run_kl_at(
    home: &TempDir,
    cwd: &TempDir,
    args: &[&str],
    stdin: &str,
    status: u16,
) -> (Output, JoinHandle<String>) {
    let (port, server) = start_mock_llm(status);
    (spawn_kl(home, cwd, args, stdin, port), server)
}

/// Run `kl` with `args` and `stdin` against the server at `port`.
fn spawn_kl(home: &TempDir, cwd: &TempDir, args: &[&str], stdin: &str, port: u16) -> Output {
    let mut child = kl(home.path())
        .args(["--model", "genai_1::test-model"])
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
    child.wait_with_output().unwrap()
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
    let names: Vec<_> = harness_core::BUNDLED_SKILLS
        .iter()
        .map(|&(name, _)| name)
        .collect();
    let hint = format!(
        "newer versions of the skills {} come with kl",
        names.join(", ")
    );
    assert!(stderr(&out).contains(&hint), "{out:?}");
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

/// The session files saved for the only folder `kl` ran in under `home`.
fn session_files(home: &TempDir) -> Vec<std::path::PathBuf> {
    let sessions = home.path().join(".kluis/sessions");
    let project = std::fs::read_dir(sessions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let mut files: Vec<_> = std::fs::read_dir(project.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
}

#[test]
fn fork_goes_on_in_a_copy_of_the_session() {
    let (home, cwd) = (tempdir(), tempdir());
    let (out, _) = run_kl_at(&home, &cwd, &["-p", "first"], "", 200);
    assert!(out.status.success(), "{out:?}");
    let files = session_files(&home);
    let [original] = files.as_slice() else {
        panic!("{files:?}");
    };
    let before = std::fs::read_to_string(original).unwrap();

    let args = ["--continue", "--fork", "--output-format", "json", "second"];
    let (out, server) = run_kl_at(&home, &cwd, &args, "", 200);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(server.join().unwrap(), "second");

    // The session continued is as it was, and the copy holds both prompts.
    assert_eq!(std::fs::read_to_string(original).unwrap(), before);
    let files = session_files(&home);
    assert_eq!(files.len(), 2, "{files:?}");
    let copy = files.iter().find(|file| *file != original).unwrap();
    let text = std::fs::read_to_string(copy).unwrap();
    let header: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let id = original.file_stem().unwrap().to_str().unwrap();
    assert_eq!(header["forked_from"], id, "{header}");
    assert!(text.contains("first") && text.contains("second"), "{text}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["session"], header["id"], "{result}");
}

#[test]
fn fork_needs_a_session_to_copy() {
    let home = tempdir();
    let out = kl(home.path()).args(["--fork", "hello"]).output().unwrap();

    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--fork"), "{err}");
}

const SCHEMA: &str = r#"{
    "type": "object",
    "properties": { "count": { "type": "integer" } },
    "required": ["count"]
}"#;

/// Run `kl` with `args` and a schema file against a server that gives
/// `answers`; return its output and the user messages the server got.
fn run_with_schema(args: &[&str], answers: &[&str]) -> (Output, Vec<String>) {
    let (home, cwd) = (tempdir(), tempdir());
    let schema = cwd.path().join("schema.json");
    std::fs::write(&schema, SCHEMA).unwrap();
    let mut all = vec!["--no-session", "--output-schema", schema.to_str().unwrap()];
    all.extend(args);
    let (port, server) = start_mock_answers(answers);
    let out = spawn_kl(&home, &cwd, &all, "", port);
    (out, server.join().unwrap())
}

#[test]
fn schema_prints_the_matching_answer_alone() {
    let (out, users) = run_with_schema(&["count"], &["```json\n{ \"count\": 3 }\n```"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(common::stdout(&out), "{\"count\":3}\n");
    let [prompt] = users.as_slice() else {
        panic!("{users:?}");
    };
    assert!(prompt.starts_with("count\n\n"), "{prompt}");
    assert!(prompt.contains(r#""required":["count"]"#), "{prompt}");
}

#[test]
fn schema_asks_once_more_for_a_matching_answer() {
    let args = ["--output-format", "json", "count"];
    let (out, users) = run_with_schema(&args, &["three", r#"{"count":3}"#]);

    assert!(out.status.success(), "{out:?}");
    assert!(users[1].contains("not JSON"), "{users:?}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["structured"], json!({ "count": 3 }));
    assert_eq!(result["result"], r#"{"count":3}"#);
    // Both answers count.
    assert_eq!(result["usage"]["output"], 6);
}

#[test]
fn schema_fails_an_answer_that_never_matches() {
    let args = ["--output-format", "stream", "count"];
    let (out, users) = run_with_schema(&args, &[r#"{"count":"3"}"#, "{}"]);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(users[1].contains("$.count: expected"), "{users:?}");
    let lines = json_lines(&out.stdout);
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "error");
    assert!(
        last["error"]
            .as_str()
            .unwrap()
            .contains("`count` is missing"),
        "{last}"
    );
}

#[test]
fn schema_with_an_unknown_keyword_is_refused() {
    let (home, cwd) = (tempdir(), tempdir());
    let schema = cwd.path().join("schema.json");
    std::fs::write(&schema, r#"{ "type": "string", "pattern": "^a" }"#).unwrap();
    let out = kl(home.path())
        .args(["--output-schema", schema.to_str().unwrap(), "hello"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).contains("`pattern` is not supported"),
        "{out:?}"
    );
}
