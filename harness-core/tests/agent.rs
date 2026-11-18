// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;
use std::time::Duration;

use common::{text, tool_calls, MockModel, Reply};
use genai::chat::{ChatMessage, ChatRole};
use harness_core::tools::ToolContext;
use harness_core::{Agent, Event};
use serde_json::{json, Value};
use tokio::sync::Notify;

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

/// Run a prompt and drop it once the model calls the tool `call_id`, while
/// that call is about to run.
async fn cancel_at_call(agent: &mut Agent, text: &str, call_id: &str) {
    let reached = Notify::new();
    let run = agent.prompt(text, |event| {
        if matches!(event, Event::ToolCall(call) if call.call_id == call_id) {
            reached.notify_one();
        }
    });
    tokio::select! {
        out = run => panic!("prompt ended before the cancel: {out:?}"),
        _ = reached.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("call never made"),
    }
}

/// The tool result answering `call_id` in the history.
fn result_of(message: &ChatMessage, call_id: &str) -> Option<String> {
    let responses = message.content.tool_responses();
    let resp = responses.iter().find(|r| r.call_id == call_id)?;
    Some(resp.content.clone())
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
                Event::TurnStart => "start".to_owned(),
                Event::TurnEnd(usage) => format!("end {:?}", usage.and_then(|u| u.total_tokens)),
                Event::TextDelta(text) => format!("delta {text}"),
                Event::Text(text) => format!("text {text}"),
                Event::ToolCall(call) => format!("call {}", call.fn_name),
                Event::ToolResult(resp) => format!("result {}", resp.call_id),
            })
        })
        .await
        .unwrap();

    assert_eq!(answer, "ok");
    assert_eq!(
        events,
        [
            "start",
            "end Some(15)",
            "call bash",
            "result call_1",
            "start",
            "delta ok",
            "text ok",
            "end Some(15)"
        ]
    );
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

#[tokio::test]
async fn cancel_during_a_tool_answers_its_call() {
    let call = ("call_1", "bash", json!({ "command": "sleep 30" }));
    let model = MockModel::start(vec![tool_calls(&[call]), text("after")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    cancel_at_call(&mut agent, "run it", "call_1").await;

    assert_eq!(
        roles(&agent),
        [ChatRole::User, ChatRole::Assistant, ChatRole::Tool]
    );
    let result = result_of(&agent.history()[2], "call_1").unwrap();
    assert!(result.contains("cancelled"), "{result}");

    // The next request answers every call, as providers require.
    assert_eq!(prompt(&mut agent, "next").await.unwrap(), "after");
    let results = sent(&model.requests()[1], "tool");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call_1");
}

#[tokio::test]
async fn cancel_keeps_finished_results() {
    let calls = [
        ("call_1", "bash", json!({ "command": "echo first" })),
        ("call_2", "bash", json!({ "command": "sleep 30" })),
    ];
    let model = MockModel::start(vec![tool_calls(&calls)]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    cancel_at_call(&mut agent, "run both", "call_2").await;

    assert_eq!(
        roles(&agent),
        [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Tool
        ]
    );
    let first = result_of(&agent.history()[2], "call_1").unwrap();
    assert!(first.contains("first"), "{first}");
    let second = result_of(&agent.history()[3], "call_2").unwrap();
    assert!(second.contains("cancelled"), "{second}");
}

#[tokio::test]
async fn cancel_while_waiting_keeps_the_user_message() {
    let model = MockModel::start(vec![Reply::Hang, text("second")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let run = agent.prompt("first", |_| {});
    let asked = async {
        while model.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        out = run => panic!("prompt ended before the cancel: {out:?}"),
        _ = asked => {}
    }

    assert_eq!(roles(&agent), [ChatRole::User]);
    assert_eq!(prompt(&mut agent, "again").await.unwrap(), "second");
    assert_eq!(sent(&model.requests()[1], "user").len(), 2);
}

#[tokio::test]
async fn text_streams_in_pieces() {
    let model = MockModel::start(vec![text("hello world")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let mut events = Vec::new();
    agent
        .prompt("hi", |event| match event {
            Event::TextDelta(text) => events.push(format!("delta {text}")),
            Event::Text(text) => events.push(format!("text {text}")),
            _ => {}
        })
        .await
        .unwrap();

    assert_eq!(
        events,
        [
            "delta hel",
            "delta lo ",
            "delta wor",
            "delta ld",
            "text hello world"
        ]
    );
    assert_eq!(agent.history()[1].content.first_text(), Some("hello world"));
}

#[tokio::test]
async fn cancel_while_streaming_drops_the_partial_reply() {
    let model =
        MockModel::start(vec![Reply::Partial("half an answer".into()), text("again")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let streaming = Notify::new();
    let run = agent.prompt("first", |event| {
        if let Event::TextDelta(_) = event {
            streaming.notify_one();
        }
    });
    tokio::select! {
        out = run => panic!("prompt ended before the cancel: {out:?}"),
        _ = streaming.notified() => {}
    }

    assert_eq!(roles(&agent), [ChatRole::User]);
    assert_eq!(prompt(&mut agent, "next").await.unwrap(), "again");
    assert!(sent(&model.requests()[1], "assistant").is_empty());
}

#[tokio::test]
async fn broken_stream_is_an_error() {
    let model = MockModel::start(vec![Reply::Broken("half an answer".into()), text("again")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let mut ended = false;
    let run = agent.prompt("first", |event| ended |= matches!(event, Event::TurnEnd(_)));
    let err = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("prompt hangs")
        .unwrap_err();
    assert!(err.to_string().contains("ended early"), "{err}");
    assert!(!ended, "a cut turn must not end");
    assert_eq!(roles(&agent), [ChatRole::User]);
    assert_eq!(prompt(&mut agent, "next").await.unwrap(), "again");
}
