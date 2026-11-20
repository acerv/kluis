// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::time::Duration;

use common::{text, tool_calls, MockModel};
use harness_core::tools::{AskTool, Question, ASK_TOOL, MAX_ASK_OPTIONS};
use harness_core::Agent;
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn ask(question: &str, options: &[&str]) -> AskTool {
    AskTool {
        question: question.to_owned(),
        options: options.iter().map(|&o| o.to_owned()).collect(),
    }
}

async fn error(tool: AskTool) -> String {
    let (asker, _questions) = mpsc::unbounded_channel();
    tool.run(Some(&asker)).await.unwrap_err().message
}

/// Names of the tools sent to the model in `request`.
fn tool_names(request: &Value) -> Vec<String> {
    let tools = request["tools"].as_array().unwrap();
    tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn definition() {
    let tool = AskTool::definition();
    assert_eq!(tool.name.as_ref(), ASK_TOOL);
    assert_eq!(tool.schema, Some(AskTool::schema()));
}

#[tokio::test]
async fn answer_comes_back() {
    let (asker, mut questions) = mpsc::unbounded_channel::<Question>();
    let user = tokio::spawn(async move {
        let question = questions.recv().await.unwrap();
        assert_eq!(question.text, "Which one?");
        assert_eq!(question.options, ["a", "b"]);
        question.answer("b");
    });

    let out = ask("Which one?", &["a", "b"])
        .run(Some(&asker))
        .await
        .unwrap();
    assert_eq!(out.answer, "b");
    user.await.unwrap();
}

#[tokio::test]
async fn dropped_question() {
    let (asker, mut questions) = mpsc::unbounded_channel::<Question>();
    let user = tokio::spawn(async move { drop(questions.recv().await) });

    let err = ask("Which one?", &[]).run(Some(&asker)).await.unwrap_err();
    assert!(err.message.contains("did not answer"), "{err}");
    user.await.unwrap();
}

#[tokio::test]
async fn no_user() {
    let err = ask("Which one?", &[]).run(None).await.unwrap_err();
    assert!(err.message.contains("no user"), "{err}");

    let (asker, questions) = mpsc::unbounded_channel();
    drop(questions);
    let err = ask("Which one?", &[]).run(Some(&asker)).await.unwrap_err();
    assert!(err.message.contains("no user"), "{err}");
}

#[tokio::test]
async fn blank_question() {
    let err = error(ask(" ", &[])).await;
    assert!(err.contains("question"), "{err}");
}

#[tokio::test]
async fn blank_option() {
    let err = error(ask("Which one?", &["a", ""])).await;
    assert!(err.contains("options must not be empty"), "{err}");
}

#[tokio::test]
async fn too_many_options() {
    let options = vec!["x"; MAX_ASK_OPTIONS + 1];
    let err = error(ask("Which one?", &options)).await;
    assert!(err.contains("options are allowed"), "{err}");
}

#[tokio::test]
async fn offered_only_with_an_asker() {
    let model = MockModel::start(vec![text("ok"), text("ok")]).await;

    let mut agent = Agent::with_client(model.client(), "test-model");
    agent.prompt("hi", |_| {}).await.unwrap();
    let (asker, _questions) = mpsc::unbounded_channel();
    let mut agent = agent.with_asker(asker.clone()).with_asker(asker);
    agent.prompt("hi", |_| {}).await.unwrap();

    let requests = model.requests();
    assert!(!tool_names(&requests[0]).iter().any(|n| n == ASK_TOOL));
    let asks = tool_names(&requests[1]);
    assert_eq!(asks.iter().filter(|n| *n == ASK_TOOL).count(), 1);
}

#[tokio::test]
async fn agent_sends_the_answer_to_the_model() {
    let call = ("call_1", ASK_TOOL, json!({ "question": "Proceed?" }));
    let model = MockModel::start(vec![tool_calls(&[call]), text("ok")]).await;
    let (asker, mut questions) = mpsc::unbounded_channel::<Question>();
    let mut agent = Agent::with_client(model.client(), "test-model").with_asker(asker);
    let user = tokio::spawn(async move { questions.recv().await.unwrap().answer("yes") });

    let run = agent.prompt("go", |_| {});
    let answer = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("prompt hangs")
        .unwrap();
    assert_eq!(answer, "ok");
    user.await.unwrap();

    let messages = model.requests()[1]["messages"].as_array().unwrap().clone();
    let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
    let output: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(output, json!({ "answer": "yes" }));
}
