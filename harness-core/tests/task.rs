// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The `task` tool: subagents working for the model.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{text, tool_calls, with_usage, MockModel};
use genai::chat::ChatRole;
use harness_core::tools::ToolContext;
use harness_core::{Agent, Event, TokenUsage};
use serde_json::{json, Value};
use tokio::sync::Notify;

/// An agent talking to `model`, with tools working in `cwd`.
fn agent(model: &MockModel, cwd: &Path) -> Agent {
    Agent::with_client(model.client(), "test-model")
        .with_system("be brief")
        .with_tool_context(ToolContext {
            cwd: cwd.to_owned(),
            ..ToolContext::default()
        })
}

/// Run a prompt, failing the test if it hangs.
async fn prompt(agent: &mut Agent, text: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), agent.prompt(text, |_| {}))
        .await
        .expect("prompt hangs")
        .unwrap()
}

/// A call of the `task` tool.
fn task(id: &'static str, prompt: &str) -> (&'static str, &'static str, Value) {
    (id, "task", json!({ "prompt": prompt }))
}

/// A call of the `task` tool for a read-only subagent.
fn read_only_task(id: &'static str, prompt: &str) -> (&'static str, &'static str, Value) {
    (id, "task", json!({ "prompt": prompt, "read_only": true }))
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

/// The names of the tools a request offers.
fn offered(request: &Value) -> Vec<&str> {
    let tools = request["tools"].as_array().unwrap();
    tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect()
}

/// The result the model got for its first tool call, in `request`.
fn result(request: &Value) -> String {
    sent(request, "tool")[0]["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn subagent_works_in_its_own_conversation_and_answers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let model = MockModel::start(vec![
        tool_calls(&[task("t1", "what is in a.txt?")]),
        with_usage(
            100,
            7,
            tool_calls(&[("r1", "read", json!({ "path": "a.txt" }))]),
        ),
        with_usage(200, 3, text("it holds alpha")),
        with_usage(40, 2, text("done")),
    ])
    .await;
    let mut agent = agent(&model, dir.path());

    let mut output = String::new();
    let mut texts = Vec::new();
    let mut main = TokenUsage::default();
    let mut sub = TokenUsage::default();
    let answer = agent
        .prompt("look at a.txt", |event| match event {
            Event::ToolOutput(chunk) => output.push_str(chunk),
            Event::Text(text) => texts.push(text.to_owned()),
            Event::TurnEnd(Some(usage)) => main += TokenUsage::from_genai(usage),
            Event::TaskUsage(usage) => sub += TokenUsage::from_genai(usage),
            _ => {}
        })
        .await
        .unwrap();
    assert_eq!(answer, "done");

    // The tool calls of the subagent show as output of the call, its
    // text does not, and its tokens are told apart.
    assert_eq!(output, "read {\"path\":\"a.txt\"}\n");
    assert_eq!(texts, ["done"]);
    assert_eq!((sub.input, sub.output), (300, 10));
    assert_eq!((main.input, main.output), (50, 7));

    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    // It starts from the prompt alone, with the same system prompt, and
    // without the tools that need the main conversation.
    assert_eq!(sent(&requests[1], "system"), sent(&requests[0], "system"));
    let users = sent(&requests[1], "user");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["content"], "what is in a.txt?");
    assert!(offered(&requests[0]).contains(&"task"));
    let tools = offered(&requests[1]);
    assert!(tools.contains(&"read"), "{tools:?}");
    for name in ["task", "todo", "ask_user"] {
        assert!(!tools.contains(&name), "{tools:?}");
    }

    // Only its answer joins the main conversation.
    assert_eq!(result(&requests[3]), r#"{"answer":"it holds alpha"}"#);
    assert!(!requests[3].to_string().contains("alpha\\n"));
    assert_eq!(
        agent
            .history()
            .iter()
            .map(|m| m.role.clone())
            .collect::<Vec<_>>(),
        [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Assistant
        ]
    );
}

#[tokio::test]
async fn each_task_starts_from_an_empty_conversation() {
    let model = MockModel::start(vec![
        tool_calls(&[task("t1", "first"), task("t2", "second")]),
        text("one"),
        text("two"),
        text("done"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    prompt(&mut agent, "go").await;

    let requests = model.requests();
    let users = sent(&requests[2], "user");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["content"], "second");
    assert!(sent(&requests[2], "assistant").is_empty());
    let results = sent(&requests[3], "tool");
    assert_eq!(results[0]["content"], r#"{"answer":"one"}"#);
    assert_eq!(results[1]["content"], r#"{"answer":"two"}"#);
}

#[tokio::test]
async fn subagent_starts_no_subagent() {
    let model = MockModel::start(vec![
        tool_calls(&[task("t1", "delegate")]),
        tool_calls(&[task("t2", "again")]),
        text("could not"),
        text("done"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    prompt(&mut agent, "go").await;

    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    let refused = result(&requests[2]);
    assert!(refused.contains("unknown tool task"), "{refused}");
}

#[tokio::test]
async fn subagent_keeps_plan_mode() {
    let model = MockModel::start(vec![
        tool_calls(&[task("t1", "write new.txt")]),
        tool_calls(&[("w1", "write", json!({ "path": "new.txt", "content": "x" }))]),
        text("not allowed"),
        text("the plan"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    agent.set_plan(true);
    prompt(&mut agent, "add new.txt").await;

    assert!(!dir.path().join("new.txt").exists());
    let requests = model.requests();
    let user = sent(&requests[1], "user")[0]["content"].to_string();
    assert!(user.contains("[Plan mode: look around"), "{user}");
    let refused = result(&requests[2]);
    assert!(refused.contains("plan mode"), "{refused}");
}

#[tokio::test]
async fn failed_task_is_an_error_the_model_reads() {
    let model = MockModel::start(vec![
        tool_calls(&[task("t1", "  "), task("t2", "say nothing")]),
        text(""),
        text("done"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    prompt(&mut agent, "go").await;

    let requests = model.requests();
    // A blank prompt starts no subagent.
    assert_eq!(requests.len(), 3);
    let results = sent(&requests[2], "tool");
    let blank = results[0]["content"].as_str().unwrap();
    assert!(blank.contains("prompt must not be empty"), "{blank}");
    let failed = results[1]["content"].as_str().unwrap();
    assert!(failed.contains("empty reply"), "{failed}");
}

#[tokio::test]
async fn cancel_stops_the_subagent_and_answers_the_task() {
    let model = MockModel::start(vec![
        tool_calls(&[task("t1", "wait")]),
        tool_calls(&[("b1", "bash", json!({ "command": "sleep 30" }))]),
        text("after"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    {
        let reached = Notify::new();
        let run = agent.prompt("go", |event| {
            if matches!(event, Event::ToolOutput(line) if line.starts_with("bash ")) {
                reached.notify_one();
            }
        });
        tokio::select! {
            out = run => panic!("prompt ended before the cancel: {out:?}"),
            _ = reached.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("call never made"),
        }
    }

    let results = agent.history()[2].content.tool_responses();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("cancelled"), "{results:?}");

    // The next request answers the call, and holds nothing of the subagent.
    assert_eq!(prompt(&mut agent, "next").await, "after");
    let request = &model.requests()[2];
    assert_eq!(sent(request, "tool").len(), 1);
    assert!(!request.to_string().contains("sleep 30"));
}

#[tokio::test]
async fn read_only_task_changes_nothing() {
    let model = MockModel::start(vec![
        tool_calls(&[read_only_task("t1", "write new.txt")]),
        tool_calls(&[("w1", "write", json!({ "path": "new.txt", "content": "x" }))]),
        text("not allowed"),
        text("done"),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    prompt(&mut agent, "go").await;

    assert!(!dir.path().join("new.txt").exists());
    let refused = result(&model.requests()[2]);
    assert!(refused.contains("read-only task"), "{refused}");
}
