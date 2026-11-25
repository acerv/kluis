// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::time::Duration;

use common::{text, tool_calls, MockModel, Reply};
use harness_core::tools::{Question, ASK_TOOL, BASH_TOOL, LS_TOOL};
use harness_core::{Agent, Permission, Permissions};
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn permissions(rules: &[(&str, Permission)]) -> Permissions {
    let mut permissions = Permissions::default();
    for &(tool, permission) in rules {
        permissions.set(tool, permission).unwrap();
    }
    permissions
}

/// A model turn calling `ls` on the current folder.
fn ls(id: &str) -> Reply {
    tool_calls(&[(id, LS_TOOL, json!({ "path": "." }))])
}

/// Names of the tools sent to the model in `request`.
fn tool_names(request: &Value) -> Vec<&str> {
    let tools = request["tools"].as_array().unwrap();
    tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect()
}

/// The results of the tool calls sent to the model in `request`.
fn results(request: &Value) -> Vec<Value> {
    let messages = request["messages"].as_array().unwrap();
    messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect()
}

/// Run a prompt on an agent asking before `ls`, whose model makes the
/// turns `replies`, with a user giving `answers`; return the requests the
/// model got and the questions the user got.
async fn run_asking(replies: Vec<Reply>, answers: &[&str]) -> (Vec<Value>, Vec<String>) {
    let model = MockModel::start(replies).await;
    let (asker, mut questions) = mpsc::unbounded_channel::<Question>();
    let mut agent = Agent::with_client(model.client(), "test-model")
        .with_permissions(permissions(&[(LS_TOOL, Permission::Ask)]))
        .with_asker(asker);
    let answers: Vec<_> = answers.iter().map(|&answer| answer.to_owned()).collect();
    let user = tokio::spawn(async move {
        let mut asked = Vec::new();
        for answer in answers {
            let question = questions.recv().await.unwrap();
            assert_eq!(question.options, ["Yes", "Always in this session", "No"]);
            asked.push(question.text.clone());
            question.answer(answer);
        }
        // Nothing else was asked once the prompt is over.
        assert!(questions.recv().await.is_none());
        asked
    });

    let run = agent.prompt("go", |_| {});
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("prompt hangs")
        .unwrap();
    drop(agent);
    (model.requests(), user.await.unwrap())
}

#[tokio::test]
async fn denied_tool_is_not_offered_and_does_not_run() {
    let model = MockModel::start(vec![ls("call_1"), text("ok")]).await;
    let mut agent = Agent::with_client(model.client(), "test-model")
        .with_permissions(permissions(&[(LS_TOOL, Permission::Deny)]));

    agent.prompt("go", |_| {}).await.unwrap();

    let requests = model.requests();
    let names = tool_names(&requests[0]);
    assert!(!names.contains(&LS_TOOL), "{names:?}");
    assert!(names.contains(&BASH_TOOL), "{names:?}");
    let error = json!({ "error": "the user does not allow `ls`" });
    assert_eq!(results(&requests[1]), [error]);
}

#[tokio::test]
async fn tool_to_ask_for_is_not_offered_with_nobody_to_ask() {
    let model = MockModel::start(vec![ls("call_1"), text("ok")]).await;
    let mut agent = Agent::with_client(model.client(), "test-model")
        .with_permissions(permissions(&[(LS_TOOL, Permission::Ask)]));

    agent.prompt("go", |_| {}).await.unwrap();

    let requests = model.requests();
    assert!(!tool_names(&requests[0]).contains(&LS_TOOL));
    assert!(results(&requests[1])[0]["error"].is_string());
}

#[tokio::test]
async fn approved_call_runs() {
    let (requests, asked) = run_asking(vec![ls("call_1"), text("ok")], &["Yes"]).await;

    assert!(tool_names(&requests[0]).contains(&LS_TOOL));
    assert_eq!(asked, ["Allow ls?\npath: ."]);
    let result = &results(&requests[1])[0];
    assert!(result.get("error").is_none(), "{result}");
}

#[tokio::test]
async fn tool_allowed_for_the_session_is_asked_for_once() {
    let replies = vec![ls("call_1"), ls("call_2"), text("ok")];
    let (requests, asked) = run_asking(replies, &["Always in this session"]).await;

    assert_eq!(asked.len(), 1);
    let results = results(&requests[2]);
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.get("error").is_none()));
}

#[tokio::test]
async fn refused_call_tells_the_model_why() {
    let replies = vec![ls("call_1"), ls("call_2"), text("ok")];
    let (requests, _) = run_asking(replies, &["No", "use find instead"]).await;

    let expected = [
        json!({ "error": "the user refused this call" }),
        json!({ "error": "the user refused this call: use find instead" }),
    ];
    assert_eq!(results(&requests[2]), expected);
}

#[tokio::test]
async fn calls_run_together_are_asked_for_one_at_a_time() {
    let both = tool_calls(&[
        ("call_1", LS_TOOL, json!({ "path": "." })),
        ("call_2", LS_TOOL, json!({ "path": ".." })),
    ]);
    let (requests, asked) = run_asking(vec![both, text("ok")], &["No", "Yes"]).await;

    assert_eq!(asked.len(), 2);
    let results = results(&requests[1]);
    assert_eq!(
        results.iter().filter(|r| r.get("error").is_some()).count(),
        1
    );
}

#[test]
fn refuses_rules_that_make_no_sense() {
    let mut permissions = Permissions::default();

    let err = permissions.set("shell", Permission::Deny).unwrap_err();
    assert_eq!(err, "unknown tool `shell`");
    assert!(permissions.set(ASK_TOOL, Permission::Ask).is_err());
    assert!(permissions.set(ASK_TOOL, Permission::Deny).is_ok());
}
