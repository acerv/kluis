// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use common::{text, MockModel};
use genai::chat::{
    ChatMessage, ChatRole, ContentPart, MessageContent, PromptTokensDetails, ToolCall,
    ToolResponse, Usage,
};
use harness_core::tools::ToolContext;
use harness_core::{Agent, SessionStore, TokenUsage};
use serde_json::json;

#[test]
fn lazy_writer_creates_no_file_until_first_message() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());

    let mut writer = store.create("deepseek-chat");
    writer.append_model("gpt-4o").unwrap();
    writer
        .append_usage(
            TokenUsage {
                input: 10,
                output: 5,
                cached: 2,
            },
            12,
        )
        .unwrap();
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
    writer
        .append_usage(
            TokenUsage {
                input: 1_200,
                output: 350,
                cached: 400,
            },
            900,
        )
        .unwrap();
    writer.append_model("claude-sonnet-4-5").unwrap();
    writer
        .append_usage(
            TokenUsage {
                input: 800,
                output: 150,
                cached: 600,
            },
            1_100,
        )
        .unwrap();
    // A prompt whose provider reported no size keeps the last one known.
    writer
        .append_usage(
            TokenUsage {
                input: 1,
                output: 0,
                cached: 0,
            },
            0,
        )
        .unwrap();

    let loaded = store.latest().unwrap().expect("session");
    assert_eq!(loaded.meta.id, writer.id());
    assert_eq!(loaded.meta.model, "claude-sonnet-4-5");
    assert_eq!(
        loaded.usage,
        TokenUsage {
            input: 2_001,
            output: 500,
            cached: 1_000,
        }
    );
    assert_eq!(loaded.context, 1_100);
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

#[test]
fn appending_to_a_whole_file_only_adds_the_new_line() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());

    let mut writer = store.create("model-a");
    writer
        .append_messages(&[ChatMessage::user("first")])
        .unwrap();
    let before = std::fs::read_to_string(writer.path()).unwrap();

    let loaded = store.latest().unwrap().expect("session");
    let mut reopened = store.open(&loaded);
    reopened
        .append_messages(&[ChatMessage::user("second")])
        .unwrap();
    let after = std::fs::read_to_string(writer.path()).unwrap();
    let added = after.strip_prefix(&before).expect("the file only grows");
    assert_eq!(added.lines().count(), 1);
    assert!(added.starts_with('{') && added.ends_with("}\n"));
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

#[test]
fn list_and_load_by_id_or_prefix_and_skip_corrupt_files() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());

    assert!(store.list().unwrap().is_empty());

    let mut w1 = store.create("model-one");
    w1.append_messages(&[
        ChatMessage::user("first\n  multiline\tprompt"),
        ChatMessage::assistant("reply 1"),
    ])
    .unwrap();

    // Exact 120-char prompt with trailing whitespace must not get a spurious ellipsis.
    let exact_120 = format!("{}\n", "a".repeat(120));
    let mut w2 = store.create("model-two");
    w2.append_messages(&[
        ChatMessage::user(exact_120),
        ChatMessage::assistant("reply 2"),
    ])
    .unwrap();
    w2.append_model("model-two-updated").unwrap();

    // A corrupt file alongside valid sessions must not break `list()`.
    std::fs::write(store.project_dir().join("broken.jsonl"), "not json\n").unwrap();

    let list = store.list().unwrap();
    assert_eq!(list.len(), 2);
    let s1 = list.iter().find(|s| s.id == w1.id()).expect("session 1");
    assert_eq!(s1.model, "model-one");
    assert_eq!(s1.message_count, 2);
    assert_eq!(s1.first_prompt.as_deref(), Some("first multiline prompt"));

    let s2 = list.iter().find(|s| s.id == w2.id()).expect("session 2");
    assert_eq!(s2.model, "model-two-updated");
    assert_eq!(s2.message_count, 2);
    assert_eq!(s2.first_prompt.as_deref(), Some("a".repeat(120).as_str()));

    // Exact ID load.
    let loaded = store.load(w1.id()).unwrap();
    assert_eq!(loaded.meta.id, w1.id());
    assert_eq!(loaded.messages.len(), 2);

    // Unique prefix load (timestamp + first 8 chars of UUID).
    let prefix = &w2.id()[..20];
    let by_prefix = store.load(prefix).unwrap();
    assert_eq!(by_prefix.meta.id, w2.id());

    // Ambiguous prefix (shared date prefix "20").
    let err = store.load("20").unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "unexpected error: {err}");

    // Missing ID & path-traversal attempts fail safely.
    assert!(store
        .load("nonexistent")
        .unwrap_err()
        .to_string()
        .contains("not found"));
    assert!(store
        .load("../../etc/passwd")
        .unwrap_err()
        .to_string()
        .contains("not found"));
    assert!(store.load("   ").unwrap_err().to_string().contains("empty"));
}

#[test]
fn token_usage_separates_cached_and_loads_legacy_entries() {
    let genai_usage = Usage {
        prompt_tokens: Some(1_000),
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: Some(750),
            ..Default::default()
        }),
        completion_tokens: Some(200),
        ..Default::default()
    };
    assert_eq!(
        TokenUsage::from_genai(&genai_usage),
        TokenUsage {
            input: 250,
            output: 200,
            cached: 750,
        }
    );

    // Defensive clamp if a provider reports cached > prompt.
    let odd_usage = Usage {
        prompt_tokens: Some(100),
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: Some(150),
            ..Default::default()
        }),
        completion_tokens: Some(50),
        ..Default::default()
    };
    assert_eq!(
        TokenUsage::from_genai(&odd_usage),
        TokenUsage {
            input: 0,
            output: 50,
            cached: 150,
        }
    );

    // Older session files without `cached_tokens` or `context_tokens` still
    // deserialize cleanly, with the size of the conversation unknown.
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());
    let mut writer = store.create("deepseek-chat");
    writer.append_messages(&[ChatMessage::user("hi")]).unwrap();
    let mut file = OpenOptions::new().append(true).open(writer.path()).unwrap();
    writeln!(
        file,
        r#"{{"type":"usage","input_tokens":500,"output_tokens":120}}"#
    )
    .unwrap();

    let loaded = store.latest().unwrap().expect("legacy session");
    assert_eq!(
        loaded.usage,
        TokenUsage {
            input: 500,
            output: 120,
            cached: 0,
        }
    );
    assert_eq!(loaded.context, 0);
}

#[test]
fn compact_entry_replaces_loaded_history_and_keeps_first_prompt() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());

    let mut writer = store.create("claude-sonnet-4-5");
    writer
        .append_messages(&[
            ChatMessage::user("original task"),
            ChatMessage::assistant("first reply"),
            ChatMessage::user("second turn"),
            ChatMessage::assistant("second reply"),
        ])
        .unwrap();
    writer
        .append_usage(
            TokenUsage {
                input: 400,
                output: 100,
                cached: 200,
            },
            600,
        )
        .unwrap();

    // Compact replaces earlier messages when loaded and resets context to 0.
    let compacted = vec![
        ChatMessage::user("Summary: original task done."),
        ChatMessage::assistant("Understood."),
    ];
    writer.append_compact(&compacted).unwrap();
    writer
        .append_usage(
            TokenUsage {
                input: 50,
                output: 30,
                cached: 550,
            },
            0,
        )
        .unwrap();

    let after_compact = store.load(writer.id()).unwrap();
    assert_eq!(after_compact.messages.len(), 2);
    assert_eq!(
        after_compact.messages[0].content.first_text(),
        Some("Summary: original task done.")
    );
    assert_eq!(after_compact.context, 0);
    assert_eq!(
        after_compact.usage,
        TokenUsage {
            input: 450,
            output: 130,
            cached: 750,
        }
    );

    // Further messages append on top of the compacted history.
    writer
        .append_messages(&[
            ChatMessage::user("next task"),
            ChatMessage::assistant("done"),
        ])
        .unwrap();
    writer
        .append_usage(
            TokenUsage {
                input: 120,
                output: 40,
                cached: 0,
            },
            120,
        )
        .unwrap();

    let loaded = store.load(writer.id()).unwrap();
    assert_eq!(loaded.messages.len(), 4);
    assert_eq!(loaded.messages[2].content.first_text(), Some("next task"));
    assert_eq!(loaded.context, 120);

    // Session summary preserves the original first_prompt and tracks post-compact count.
    let summary = &store.list().unwrap()[0];
    assert_eq!(summary.first_prompt.as_deref(), Some("original task"));
    assert_eq!(summary.message_count, 4);
}

#[test]
fn images_are_left_out_of_the_file() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());
    let mut writer = store.create("deepseek-chat");

    let image = ContentPart::Binary(genai::chat::Binary::from_base64("image/png", "AAAA", None));
    let message = ChatMessage::user(MessageContent::from_parts(vec![
        ContentPart::Text("see".to_owned()),
        image,
    ]));
    writer.append_messages(&[message]).unwrap();

    let loaded = store.latest().unwrap().expect("saved session");
    let content = &loaded.messages[0].content;
    assert!(!content.contains_binary());
    assert_eq!(content.texts(), ["see", "[image not saved]"]);
}

#[test]
fn compaction_rewrites_the_file_without_the_replaced_messages() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());
    let mut writer = store.create("deepseek-chat");
    writer
        .append_messages(&[
            ChatMessage::user("original task"),
            ChatMessage::assistant("a long reply"),
        ])
        .unwrap();
    writer.append_model("gpt-4o").unwrap();

    writer
        .append_compact(&[ChatMessage::user("summary"), ChatMessage::assistant("ok")])
        .unwrap();
    writer
        .append_messages(&[ChatMessage::user("next")])
        .unwrap();

    let file = std::fs::read_to_string(writer.path()).unwrap();
    assert!(!file.contains("a long reply"), "{file}");
    let dir = writer.path().parent().unwrap();
    assert!(!dir.read_dir().unwrap().any(|e| {
        let name = e.unwrap().file_name();
        name.to_string_lossy().ends_with(".tmp")
    }));
    let loaded = store.load(writer.id()).unwrap();
    assert_eq!(loaded.meta.model, "gpt-4o");
    let texts: Vec<_> = loaded
        .messages
        .iter()
        .map(|m| m.content.first_text().unwrap())
        .collect();
    assert_eq!(texts, ["summary", "ok", "next"]);
    let summary = &store.list().unwrap()[0];
    assert_eq!(summary.first_prompt.as_deref(), Some("original task"));
    assert_eq!(summary.message_count, 3);
}

#[test]
fn going_back_to_no_messages_is_saved() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());
    let mut writer = store.create("deepseek-chat");
    writer.append_compact(&[]).unwrap();
    assert!(!writer.path().exists());

    writer
        .append_messages(&[ChatMessage::user("task"), ChatMessage::assistant("done")])
        .unwrap();
    writer.append_compact(&[]).unwrap();
    writer
        .append_messages(&[ChatMessage::user("task again")])
        .unwrap();

    let loaded = store.load(writer.id()).unwrap();
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].content.first_text(), Some("task again"));
}

#[test]
fn fork_copies_the_conversation_to_a_new_session() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path(), cwd.path());
    let usage = TokenUsage {
        input: 10,
        output: 5,
        cached: 2,
    };
    let messages = [ChatMessage::user("hello"), ChatMessage::assistant("hi")];

    let mut original = store.create("deepseek-chat");
    original.append_messages(&messages).unwrap();
    original.append_usage(usage, 12).unwrap();
    let before = std::fs::read(original.path()).unwrap();

    let mut fork = store
        .fork(original.id(), "gpt-4o", &messages, usage, 12)
        .unwrap();
    assert_ne!(fork.id(), original.id());
    fork.append_messages(&[ChatMessage::user("another way")])
        .unwrap();

    // The copy has it all, and says where it comes from.
    let loaded = store.load(fork.id()).unwrap();
    assert_eq!(loaded.meta.forked_from.as_deref(), Some(original.id()));
    assert_eq!(loaded.meta.model, "gpt-4o");
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(loaded.usage, usage);
    assert_eq!(loaded.context, 12);

    // The session it copies is untouched.
    assert_eq!(std::fs::read(original.path()).unwrap(), before);
    let loaded = store.load(original.id()).unwrap();
    assert_eq!(loaded.meta.forked_from, None);
    assert_eq!(loaded.messages.len(), 2);

    // The list tells them apart, and reopening the copy keeps its origin.
    let list = store.list().unwrap();
    let of = |id: &str| list.iter().find(|s| s.id == id).unwrap();
    assert_eq!(of(fork.id()).forked_from.as_deref(), Some(original.id()));
    assert_eq!(of(fork.id()).first_prompt.as_deref(), Some("hello"));
    assert_eq!(of(original.id()).forked_from, None);
    let mut reopened = store.open(&store.load(fork.id()).unwrap());
    reopened.append_compact(&messages[..1]).unwrap();
    let loaded = store.load(fork.id()).unwrap();
    assert_eq!(loaded.meta.forked_from.as_deref(), Some(original.id()));
    assert_eq!(loaded.messages.len(), 1);
}
