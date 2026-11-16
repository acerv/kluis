mod common;

use common::p;
use harness_tools::{execute, ReadParams, ToolCall, ToolError, ToolOutput, WriteParams};
use serde_json::json;

#[test]
fn unknown_tool_fails() {
    let r = serde_json::from_value::<ToolCall>(json!({"name":"bash","input":{"cmd":"ls"}}));
    assert!(r.is_err());
}

#[test]
fn error_json_and_display() {
    let err = ToolError::new("a.txt: boom");
    let v = serde_json::to_value(&err).unwrap();
    assert_eq!(v, json!({"message":"a.txt: boom"}));
    assert_eq!(serde_json::from_value::<ToolError>(v).unwrap(), err);
    assert_eq!(err.to_string(), "a.txt: boom");
}

#[tokio::test]
async fn read_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("e.txt");
    std::fs::write(&f, "a\nb\n").unwrap();

    let out = execute(ToolCall::Read(ReadParams {
        path: p(&f),
        offset: None,
        limit: None,
    }))
    .await
    .unwrap();
    match out {
        ToolOutput::Read(r) => {
            assert_eq!(r.content, "a\nb\n");
            assert_eq!(r.total_lines, 2);
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn read_error_propagates() {
    let dir = tempfile::tempdir().unwrap();

    let err = execute(ToolCall::Read(ReadParams {
        path: p(&dir.path().join("missing")),
        offset: None,
        limit: None,
    }))
    .await
    .unwrap_err();
    assert!(err.message.contains("missing"));
}

#[tokio::test]
async fn write_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("e.txt");

    let out = execute(ToolCall::Write(WriteParams {
        path: p(&f),
        content: "a\nb\n".into(),
    }))
    .await
    .unwrap();
    assert!(matches!(out, ToolOutput::Write(w) if w.bytes_written == 4));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nb\n");
}

#[tokio::test]
async fn write_error_propagates() {
    let dir = tempfile::tempdir().unwrap();

    let err = execute(ToolCall::Write(WriteParams {
        path: p(dir.path()),
        content: "x".into(),
    }))
    .await
    .unwrap_err();
    assert!(err.message.contains(&p(dir.path())));
}
