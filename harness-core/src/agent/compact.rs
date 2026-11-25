// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Making the conversation shorter: a summary replaces it, or the old
//! tool outputs are dropped.

use std::borrow::Cow;

use genai::chat::{ChatMessage, ChatRole, ContentPart};

use super::Agent;
use crate::{Error, Event};

impl Agent {
    /// Ask the model to summarize the conversation so far and replace the
    /// history with a compact handoff, freeing context tokens for subsequent
    /// prompts.
    ///
    /// An optional `focus` steers what the summary should preserve. If the
    /// request fails, is cancelled, or returns empty text, the existing
    /// history is left untouched.
    pub async fn compact(
        &mut self,
        focus: Option<&str>,
        mut on_event: impl FnMut(Event<'_>),
    ) -> Result<String, Error> {
        self.compact_inner(focus, false, &mut on_event).await
    }

    pub(super) async fn run_auto_compact(
        &mut self,
        context_tokens: u64,
        continue_task: bool,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> bool {
        let before = self.req.messages.len();
        on_event(Event::Compacting { context_tokens });
        match self.compact_inner(None, continue_task, on_event).await {
            Ok(_) => {
                on_event(Event::Compacted {
                    before,
                    messages: &self.req.messages,
                });
                true
            }
            Err(err) => {
                let msg = err.to_string();
                on_event(Event::CompactFailed(&msg));
                false
            }
        }
    }

    async fn compact_inner(
        &mut self,
        focus: Option<&str>,
        continue_task: bool,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> Result<String, Error> {
        if self.req.messages.is_empty() {
            return Err("nothing to compact".into());
        }
        let instruction = compact_instruction(focus);
        on_event(Event::TurnStart);
        let turn = self
            .next_turn_retrying(Some(&instruction), on_event)
            .await?;

        let mut raw = turn.content.joined_texts().unwrap_or_default();
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            on_event(Event::Text(trimmed));
        }
        on_event(Event::TurnEnd(turn.usage.as_ref()));

        if trimmed.is_empty() {
            return Err("model returned an empty summary".into());
        }
        let mut compacted = Vec::with_capacity(2 + usize::from(continue_task));
        compacted.push(ChatMessage::user(format!("{COMPACT_HEADER}\n\n{trimmed}")));
        compacted.push(ChatMessage::assistant(COMPACT_ACK));
        if continue_task {
            compacted.push(ChatMessage::user(COMPACT_CONTINUE));
        }
        self.set_history(compacted);
        let end = raw.trim_end().len();
        let start = raw.len() - raw.trim_start().len();
        raw.truncate(end);
        if start > 0 {
            raw.drain(..start);
        }
        Ok(raw)
    }
}

/// Output of an old tool call removed to fit the context window.
const PRUNED: &str =
    r#"{"error":"output removed to fit the context window; run the tool again if needed"}"#;

/// An old image removed to fit the context window.
const PRUNED_IMAGE: &str = "[image removed to fit the context window]";

const COMPACT_PROMPT: &str = "Summarize the conversation so far as a concise handoff so work can \
continue seamlessly without the earlier messages. Do not call any tools. Include:\n\
- The user's goal and key decisions or constraints\n\
- Files inspected, created, or modified (with exact paths)\n\
- Important findings, errors, or command results\n\
- Remaining tasks and next steps";

pub(super) const COMPACT_HEADER: &str = "Summary of the earlier conversation:";
const COMPACT_ACK: &str = "Understood. Continuing from the compacted summary.";
pub(super) const COMPACT_CONTINUE: &str =
    "Continue with the task from the compacted summary above.";

/// Whether to compact a conversation that is `context` tokens long (0
/// when unknown), with `limit` the size to compact at. Once it is, it is
/// not again while `blocked`, which lasts until the conversation got
/// shorter than the limit: a summary that is still too long would
/// otherwise be summarized over and over.
pub(super) fn is_due(limit: Option<u64>, context: u64, blocked: &mut bool) -> bool {
    match limit {
        Some(limit) if context >= limit => !*blocked,
        Some(_) if context > 0 => {
            *blocked = false;
            false
        }
        _ => false,
    }
}

fn compact_instruction(focus: Option<&str>) -> Cow<'static, str> {
    match focus.map(str::trim).filter(|s| !s.is_empty()) {
        Some(focus) => Cow::Owned(format!(
            "{COMPACT_PROMPT}\n\nPay particular attention to: {focus}"
        )),
        None => Cow::Borrowed(COMPACT_PROMPT),
    }
}

/// Replace the outputs of the tool calls before the last model turn with
/// [`PRUNED`], returning how many were replaced. The last turn's outputs
/// stay: the model needs them to go on.
pub(super) fn prune_tool_outputs(messages: &mut [ChatMessage]) -> usize {
    let Some(last) = messages.iter().rposition(|m| m.role == ChatRole::Assistant) else {
        return 0;
    };
    let mut pruned = 0;
    for part in messages[..last]
        .iter_mut()
        .flat_map(|m| m.content.iter_mut())
    {
        match part {
            ContentPart::ToolResponse(resp) if resp.content.len() > PRUNED.len() => {
                resp.content = PRUNED.to_owned();
            }
            ContentPart::Binary(_) => *part = ContentPart::Text(PRUNED_IMAGE.to_owned()),
            _ => continue,
        }
        pruned += 1;
    }
    pruned
}
