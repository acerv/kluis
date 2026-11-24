// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What the agent reports, as the app takes it.

use std::time::Duration;

use harness_core::{Event as AgentEvent, TokenUsage, ToolDetails};

/// Owned agent progress update passed over the channel from [`Agent::prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentUpdate {
    TurnStart,
    ReasoningDelta(String),
    TextDelta(String),
    Text(String),
    TurnEnd(TokenUsage),
    /// A turn of a subagent ended: its tokens count, not its context.
    TaskUsage(TokenUsage),
    /// The turn failed for a passing reason and will be asked again.
    Retry {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    ToolOutput(String),
    ToolResult {
        content: String,
        details: Option<ToolDetails>,
    },
    Compacting(u64),
    Compacted(usize),
    CompactFailed(String),
    /// The request was too large: the outputs of this many old tool calls
    /// were removed.
    Pruned(usize),
    /// The final text of the model was cut at its token limit.
    Truncated,
    /// A message the user queued joined the conversation.
    Steered(String),
}

impl AgentUpdate {
    /// Convert a borrowed [`harness_core::Event`] into an owned update.
    pub fn from_event(event: AgentEvent<'_>) -> Option<Self> {
        Some(match event {
            AgentEvent::Messages(_) => return None,
            AgentEvent::TurnStart => Self::TurnStart,
            AgentEvent::Truncated => Self::Truncated,
            AgentEvent::ReasoningDelta(text) => Self::ReasoningDelta(text.to_owned()),
            AgentEvent::TextDelta(text) => Self::TextDelta(text.to_owned()),
            AgentEvent::Text(text) => Self::Text(text.to_owned()),
            AgentEvent::TurnEnd(usage) => Self::TurnEnd(TokenUsage::from_genai(usage?)),
            AgentEvent::TaskUsage(usage) => Self::TaskUsage(TokenUsage::from_genai(usage)),
            AgentEvent::Retry {
                attempt,
                delay,
                reason,
            } => Self::Retry {
                attempt,
                delay,
                reason: reason.to_owned(),
            },
            AgentEvent::ToolCall(call) => Self::ToolCall {
                name: call.fn_name.clone(),
                args: call.fn_arguments.to_string(),
            },
            AgentEvent::ToolOutput(chunk) => Self::ToolOutput(chunk.to_owned()),
            AgentEvent::ToolResult(resp, details) => Self::ToolResult {
                content: resp.content.clone(),
                details: details.cloned(),
            },
            AgentEvent::Compacting { context_tokens } => Self::Compacting(context_tokens),
            AgentEvent::Compacted { before, .. } => Self::Compacted(before),
            AgentEvent::CompactFailed(err) => Self::CompactFailed(err.to_owned()),
            AgentEvent::Pruned(count) => Self::Pruned(count),
            AgentEvent::Steered(text) => Self::Steered(text.to_owned()),
        })
    }
}
