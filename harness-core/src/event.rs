// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What happens while the agent works on a prompt.

use std::time::Duration;

use genai::chat::{ChatMessage, ToolCall, ToolResponse, Usage};

use crate::tools::ToolDetails;

/// Progress reported by [`Agent::prompt`](crate::Agent::prompt).
///
/// Events borrow from the agent's history to avoid copies; clone what you
/// need to keep beyond the callback.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// Messages that joined the history since the last such event of the
    /// prompt, sent before every turn so they can be saved while it runs.
    /// The ones after the last event are left to read from the history.
    Messages(&'a [ChatMessage]),
    /// A message the user queued with [`Steering`](crate::Steering) joined
    /// the conversation.
    Steered(&'a str),
    /// The agent is asking the model for its next turn.
    TurnStart,
    /// A piece of the model's reasoning, as it arrives, from models that
    /// think before they answer.
    ReasoningDelta(&'a str),
    /// A piece of the text the model is writing, as it arrives.
    TextDelta(&'a str),
    /// The whole text the model wrote in a turn, once the turn is done.
    Text(&'a str),
    /// The model's turn failed for a passing reason, such as a rate limit
    /// or a dropped connection: the agent waits `delay`, then asks again.
    /// Text streamed by the failed attempt is not part of the turn.
    Retry {
        /// Retry number, from 1.
        attempt: u32,
        delay: Duration,
        reason: &'a str,
    },
    /// The model finished its turn, with the tokens it used when the
    /// provider reports them. Not sent for a turn cut short.
    TurnEnd(Option<&'a Usage>),
    /// The tokens a turn of a subagent used, while a `task` call runs.
    /// They add to the tokens of the prompt, not to its context.
    TaskUsage(&'a Usage),
    /// The final text of the model was cut at its token limit.
    Truncated,
    /// A tool call the model made, before it runs.
    ToolCall(&'a ToolCall),
    /// A cleaned chunk of live output from a running tool (e.g. `bash`),
    /// or a line for each tool call the subagent of a `task` call makes.
    ToolOutput(&'a str),
    /// The result of a tool call, as sent back to the model, with optional
    /// UI-only details (such as a diff for `edit`).
    ToolResult(&'a ToolResponse, Option<&'a ToolDetails>),
    /// Context usage crossed the auto-compaction threshold and the agent is
    /// summarizing the conversation.
    Compacting { context_tokens: u64 },
    /// Auto-compaction replaced `before` messages with `messages`, now the
    /// whole history.
    Compacted {
        before: usize,
        messages: &'a [ChatMessage],
    },
    /// Auto-compaction failed and left the existing history untouched.
    CompactFailed(&'a str),
    /// The request was too large for the model: the outputs of this many
    /// tool calls before the last turn were removed from the history, and
    /// the model is asked again.
    Pruned(usize),
}
