// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use genai::chat::{ToolCall, ToolResponse};
use harness_core::tools::{definitions, run};
use serde_json::{json, Value};

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        call_id: "call_1".into(),
        fn_name: name.into(),
        fn_arguments: args,
        thought_signatures: None,
    }
}

fn content(resp: &ToolResponse) -> Value {
    serde_json::from_str(&resp.content).unwrap()
}

#[tokio::test]
async fn response_matches_call() {
    let resp = run(&call("bash", json!({}))).await;
    assert_eq!(resp.call_id, "call_1");
    assert_eq!(resp.fn_name.as_deref(), Some("bash"));
}

#[tokio::test]
async fn unknown_tool() {
    let resp = run(&call("bash", json!({"cmd": "ls"}))).await;
    assert_eq!(content(&resp), json!({"error": "unknown tool bash"}));
}

#[tokio::test]
async fn invalid_arguments() {
    let resp = run(&call("read", json!({"offset": 1}))).await;
    let err = content(&resp)["error"].as_str().unwrap().to_owned();
    assert!(err.contains("path"), "{err}");
}

#[tokio::test]
async fn tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");

    let resp = run(&call("read", json!({ "path": missing }))).await;
    let err = content(&resp)["error"].as_str().unwrap().to_owned();
    assert!(err.contains("missing"), "{err}");
}

#[tokio::test]
async fn runs_read() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "hi\n").unwrap();

    let resp = run(&call("read", json!({ "path": f }))).await;
    assert_eq!(
        content(&resp),
        json!({"content": "hi\n", "total_lines": 1, "truncated": false})
    );
}

#[tokio::test]
async fn runs_write() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");

    let resp = run(&call("write", json!({ "path": f, "content": "hi\n" }))).await;
    assert_eq!(content(&resp), json!({"bytes_written": 3}));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "hi\n");
}

#[tokio::test]
async fn every_definition_is_dispatched() {
    for tool in definitions() {
        let name = tool.name.as_ref();
        let resp = run(&call(name, json!({}))).await;
        let err = content(&resp)["error"].as_str().unwrap().to_owned();
        assert!(!err.starts_with("unknown tool"), "{name}: {err}");
    }
}
