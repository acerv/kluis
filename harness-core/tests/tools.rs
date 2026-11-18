// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::sync::Arc;

use genai::chat::{ToolCall, ToolResponse};
use harness_core::tools::{definitions, run, run_with, BashContext, ToolContext};
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
    let resp = run(&call("fly", json!({}))).await;
    assert_eq!(resp.call_id, "call_1");
    assert_eq!(resp.fn_name.as_deref(), Some("fly"));
}

#[tokio::test]
async fn unknown_tool() {
    let resp = run(&call("fly", json!({"to": "moon"}))).await;
    assert_eq!(content(&resp), json!({"error": "unknown tool fly"}));
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
async fn runs_edit() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "hi\n").unwrap();

    let args = json!({ "path": f, "edits": [{"oldText": "hi", "newText": "bye"}] });
    let resp = run(&call("edit", args)).await;
    assert_eq!(content(&resp), json!({"replacements": 1}));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "bye\n");
}

#[tokio::test]
async fn runs_bash() {
    let resp = run(&call("bash", json!({ "command": "echo hi" }))).await;
    assert_eq!(
        content(&resp),
        json!({
            "exit_code": 0,
            "output": "hi\n",
            "truncated": false,
            "timed_out": false
        })
    );
}

#[tokio::test]
async fn bash_uses_tool_context() {
    // EPERM from the hook stops the command before it runs.
    let ctx = ToolContext {
        bash: BashContext {
            pre_exec: Some(Arc::new(|| Err(std::io::Error::from_raw_os_error(1)))),
            ..BashContext::default()
        },
    };

    let resp = run_with(&call("bash", json!({ "command": "echo hi" })), &ctx).await;
    let err = content(&resp)["error"].as_str().unwrap().to_owned();
    assert!(err.contains("Operation not permitted"), "{err}");
}

#[tokio::test]
async fn every_definition_is_dispatched() {
    for tool in definitions() {
        let name = tool.name.as_ref();
        let resp = run(&call(name, json!({}))).await;
        // Tools with no required argument succeed on an empty call.
        if let Some(err) = content(&resp)["error"].as_str() {
            assert!(!err.starts_with("unknown tool"), "{name}: {err}");
        }
    }
}
