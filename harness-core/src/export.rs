// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A conversation written out as Markdown, for `/export`.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use genai::chat::{ChatMessage, ChatRole, ContentPart};
use rustix::fs::OFlags;

/// The conversation `messages` as Markdown: a section for each message of
/// the user and of the model, the tool calls with their arguments and
/// results in code blocks. Reasoning is left out, images are named only.
pub fn markdown(messages: &[ChatMessage]) -> String {
    let mut out = String::from("# Conversation\n");
    for message in messages {
        let heading = match message.role {
            ChatRole::System => "System",
            ChatRole::User => "User",
            ChatRole::Assistant => "Assistant",
            // Tool results follow the calls of the model's section.
            ChatRole::Tool => "",
        };
        if !heading.is_empty() {
            let _ = write!(out, "\n## {heading}\n");
        }
        for part in message.content.parts() {
            match part {
                ContentPart::Text(text) if !text.trim().is_empty() => {
                    let _ = write!(out, "\n{}\n", text.trim_end());
                }
                ContentPart::Binary(binary) => {
                    let _ = write!(out, "\n*[{}]*\n", binary.content_type);
                }
                ContentPart::ToolCall(call) => {
                    let args = serde_json::to_string_pretty(&call.fn_arguments)
                        .unwrap_or_else(|_| call.fn_arguments.to_string());
                    let _ = write!(out, "\n**Tool call:** `{}`\n\n", call.fn_name);
                    code_block(&mut out, "json", &args);
                }
                ContentPart::ToolResponse(resp) => {
                    out.push_str("\n**Tool result:**\n\n");
                    code_block(&mut out, "", &resp.content);
                }
                _ => {}
            }
        }
    }
    out
}

/// Write the conversation `messages` as Markdown to `path`, replacing the
/// file there.
///
/// The working folder is writable by the model: a symlink or a FIFO it
/// left at `path` is refused rather than followed or waited on.
pub fn write_markdown(path: &Path, messages: &[ChatMessage]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    file.set_len(0)?;
    file.write_all(markdown(messages).as_bytes())
}

/// Append `text` to `out` as a fenced code block, with a fence longer than
/// any run of backticks in it.
fn code_block(out: &mut String, lang: &str, text: &str) {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest.max(2) + 1);
    let _ = writeln!(
        out,
        "{fence}{lang}\n{}\n{fence}",
        text.trim_end_matches('\n')
    );
}
