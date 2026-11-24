// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::os::unix::fs::symlink;

use genai::chat::{Binary, ChatMessage, ContentPart, MessageContent, ToolCall, ToolResponse};
use harness_core::export::{markdown, write_markdown};
use serde_json::json;

fn conversation() -> Vec<ChatMessage> {
    let call = ToolCall {
        call_id: "c1".to_owned(),
        fn_name: "bash".to_owned(),
        fn_arguments: json!({ "command": "ls" }),
        thought_signatures: None,
    };
    vec![
        ChatMessage::user(MessageContent::from_parts(vec![
            ContentPart::Text("what is here?".to_owned()),
            ContentPart::Binary(Binary::from_base64("image/png", "AAAA", None)),
        ])),
        ChatMessage::assistant(MessageContent::from_parts(vec![
            ContentPart::ReasoningContent("secret thoughts".to_owned()),
            ContentPart::Text("Let me look.".to_owned()),
            ContentPart::ToolCall(call),
        ])),
        ChatMessage::from(ToolResponse::new("c1", "a.txt\n```b```\n")),
        ChatMessage::assistant("Two files."),
    ]
}

#[test]
fn markdown_has_a_section_for_each_turn() {
    let md = markdown(&conversation());
    assert_eq!(
        md,
        "# Conversation\n\
         \n## User\n\nwhat is here?\n\n*[image/png]*\n\
         \n## Assistant\n\nLet me look.\n\n**Tool call:** `bash`\n\n\
         ```json\n{\n  \"command\": \"ls\"\n}\n```\n\
         \n**Tool result:**\n\n````\na.txt\n```b```\n````\n\
         \n## Assistant\n\nTwo files.\n"
    );
}

#[test]
fn write_markdown_replaces_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.md");
    std::fs::write(&path, "x".repeat(10_000)).unwrap();
    write_markdown(&path, &conversation()).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        markdown(&conversation())
    );
}

#[test]
fn write_markdown_refuses_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, "keep").unwrap();
    let path = dir.path().join("out.md");
    symlink(&target, &path).unwrap();
    assert!(write_markdown(&path, &conversation()).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
}

#[test]
fn write_markdown_refuses_a_fifo() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.md");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    assert!(write_markdown(&path, &conversation()).is_err());
}
