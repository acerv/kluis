// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;
use std::time::Duration;

use common::{text, tool_calls, MockModel};
use genai::chat::ChatRole;
use harness_core::tools::ToolContext;
use harness_core::{Agent, Event};
use serde_json::{json, Value};

fn assert_send<T: Send>(_: T) {}

/// An agent talking to `model`, with tools working in `cwd`.
fn agent(model: &MockModel, cwd: &Path) -> Agent {
    Agent::with_client(model.client(), "test-model").with_tool_context(ToolContext {
        cwd: cwd.to_owned(),
        ..ToolContext::default()
    })
}

fn roles(agent: &Agent) -> Vec<ChatRole> {
    agent.history().iter().map(|m| m.role.clone()).collect()
}

/// The messages of a request sent to the model with `role`.
fn sent(request: &Value, role: &str) -> Vec<Value> {
    let messages = request["messages"].as_array().unwrap();
    messages
        .iter()
        .filter(|m| m["role"] == role)
        .cloned()
        .collect()
}

/// Run a prompt, failing the test if it hangs.
async fn prompt(agent: &mut Agent, text: &str) -> Result<String, harness_core::Error> {
    tokio::time::timeout(Duration::from_secs(10), agent.prompt(text, |_| {}))
        .await
        .expect("prompt hangs")
}

/// A UI runs the agent on its own task, which needs a `Send` future.
#[test]
fn prompt_future_is_send() {
    let mut agent = Agent::new("deepseek-chat").unwrap();
    assert_send(agent.prompt("hi", |_| {}));
}

#[test]
fn history_starts_empty() {
    let agent = Agent::new("deepseek-chat").unwrap();
    assert!(agent.history().is_empty());
}

#[tokio::test]
async fn text_answer() {
    let model = MockModel::start(vec![text("done")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    assert_eq!(prompt(&mut agent, "hi").await.unwrap(), "done");
    assert_eq!(roles(&agent), [ChatRole::User, ChatRole::Assistant]);
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test]
async fn tool_call_round_trip() {
    let call = ("call_1", "bash", json!({ "command": "echo hi" }));
    let model = MockModel::start(vec![tool_calls(&[call]), text("ok")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let mut events = Vec::new();
    let answer = agent
        .prompt("run it", |event| {
            events.push(match event {
                Event::Text(text) => format!("text {text}"),
                Event::ToolCall(call) => format!("call {}", call.fn_name),
                Event::ToolResult(resp) => format!("result {}", resp.call_id),
            })
        })
        .await
        .unwrap();

    assert_eq!(answer, "ok");
    assert_eq!(events, ["call bash", "result call_1", "text ok"]);
    assert_eq!(
        roles(&agent),
        [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Assistant
        ]
    );
    let results = sent(&model.requests()[1], "tool");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call_1");
    let output: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(output["output"], "hi\n");
}

#[tokio::test]
async fn too_many_turns_keeps_history_valid() {
    let call = ("call_1", "bash", json!({ "command": "true" }));
    let model = MockModel::start(vec![tool_calls(&[call])]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path()).with_max_turns(1);

    let err = prompt(&mut agent, "loop").await.unwrap_err();
    assert!(err.to_string().contains("stopped after 1 turns"), "{err}");
    assert_eq!(
        roles(&agent),
        [ChatRole::User, ChatRole::Assistant, ChatRole::Tool]
    );
}
