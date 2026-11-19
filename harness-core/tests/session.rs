// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use common::{text, MockModel};
use genai::chat::{ChatMessage, ChatRole, ContentPart, MessageContent, ToolCall, ToolResponse};
use harness_core::tools::ToolContext;
use harness_core::{Agent, SessionStore};
use serde_json::json;

#[test]
fn lazy_writer_creates_no_file_until_first_message() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());

    let mut writer = store.create("deepseek-chat");
    writer.append_model("gpt-4o").unwrap();
    writer.append_usage(10, 5).unwrap();
    assert!(!writer.path().exists());
    assert!(store.latest().unwrap().is_none());

    writer
        .append_messages(&[ChatMessage::user("hello")])
        .unwrap();
    assert!(writer.path().exists());

    // Session files must be private (0600).
    let mode = std::fs::metadata(writer.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "unexpected session file mode: {mode:o}");

    let loaded = store.latest().unwrap().expect("saved session");
    assert_eq!(loaded.meta.model, "gpt-4o");
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].content.first_text(), Some("hello"));
}

#[test]
fn round_trip_preserves_messages_tools_reasoning_usage_and_model() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());

    let mut writer = store.create("deepseek-chat");
    let assistant_turn = ChatMessage::assistant(MessageContent::from_parts(vec![
        ContentPart::ReasoningContent("check main.rs".into()),
        ContentPart::ToolCall(ToolCall {
            call_id: "call_1".into(),
            fn_name: "read".into(),
            fn_arguments: json!({ "path": "src/main.rs" }),
            thought_signatures: None,
        }),
    ]));
    let tool_turn = ChatMessage::from(ToolResponse::new("call_1", r#"{"content":"fn main(){}"}"#));
    let final_turn = ChatMessage::assistant("All set.");

    writer
        .append_messages(&[
            ChatMessage::user("inspect main"),
            assistant_turn,
            tool_turn,
            final_turn,
        ])
        .unwrap();
    writer.append_usage(1_200, 350).unwrap();
    writer.append_model("claude-sonnet-4-5").unwrap();
    writer.append_usage(800, 150).unwrap();

    let loaded = store.latest().unwrap().expect("session");
    assert_eq!(loaded.meta.id, writer.id());
    assert_eq!(loaded.meta.model, "claude-sonnet-4-5");
    assert_eq!(loaded.input_tokens, 2_000);
    assert_eq!(loaded.output_tokens, 500);
    assert_eq!(loaded.messages.len(), 4);
    assert_eq!(loaded.messages[0].role, ChatRole::User);
    assert_eq!(loaded.messages[1].role, ChatRole::Assistant);
    assert_eq!(loaded.messages[1].content.tool_calls().len(), 1);
    assert_eq!(loaded.messages[2].role, ChatRole::Tool);
    assert_eq!(loaded.messages[3].content.first_text(), Some("All set."));

    // Reopening appends cleanly to the same session file.
    let mut reopened = store.open(&loaded);
    reopened
        .append_messages(&[ChatMessage::user("follow up")])
        .unwrap();
    let reloaded = store.latest().unwrap().expect("reloaded");
    assert_eq!(reloaded.messages.len(), 5);
    assert_eq!(reloaded.messages[4].content.first_text(), Some("follow up"));
}

#[test]
fn recovers_from_truncated_trailing_line_and_scopes_by_project() {
    let root = tempfile::tempdir().unwrap();
    let proj_a = tempfile::tempdir().unwrap();
    let proj_b = tempfile::tempdir().unwrap();

    let store_a = SessionStore::new(root.path(), proj_a.path());
    let store_b = SessionStore::new(root.path(), proj_b.path());
    assert_ne!(store_a.project_dir(), store_b.project_dir());

    let mut writer = store_a.create("model-a");
    writer
        .append_messages(&[ChatMessage::user("first"), ChatMessage::assistant("reply")])
        .unwrap();
    // Simulate a SIGKILL mid-write leaving a truncated JSON line at EOF.
    let mut file = OpenOptions::new().append(true).open(writer.path()).unwrap();
    file.write_all(b"{\"type\":\"message\",\"message\":{\"role\":\"Us")
        .unwrap();
    drop(file);

    assert!(store_b.latest().unwrap().is_none());
    let loaded = store_a.latest().unwrap().expect("recovered session");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[1].content.first_text(), Some("reply"));

    // Appending after crash recovery must repair the truncated tail so a
    // subsequent reload succeeds cleanly.
    let mut reopened = store_a.open(&loaded);
    reopened
        .append_messages(&[ChatMessage::user("after crash")])
        .unwrap();
    let reloaded = store_a.latest().unwrap().expect("reloaded after repair");
    assert_eq!(reloaded.messages.len(), 3);
    assert_eq!(
        reloaded.messages[2].content.first_text(),
        Some("after crash")
    );
}

#[tokio::test]
async fn agent_with_history_sends_restored_turns_to_model() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());

    let mut writer = store.create("test-model");
    writer
        .append_messages(&[
            ChatMessage::user("remember 42"),
            ChatMessage::assistant("remembered"),
        ])
        .unwrap();
    let loaded = store.latest().unwrap().expect("session");

    let model = MockModel::start(vec![text("42")]).await;
    let mut agent = Agent::with_client(model.client(), &loaded.meta.model)
        .with_tool_context(ToolContext {
            cwd: cwd.path().to_owned(),
            ..ToolContext::default()
        })
        .with_history(loaded.messages);

    let answer = agent.prompt("what number?", |_| {}).await.unwrap();
    assert_eq!(answer, "42");

    let reqs = model.requests();
    let msgs = reqs[0]["messages"].as_array().unwrap();
    let contents: Vec<&str> = msgs.iter().filter_map(|m| m["content"].as_str()).collect();
    assert_eq!(contents, vec!["remember 42", "remembered", "what number?"]);
}
