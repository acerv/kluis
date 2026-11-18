// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What happens while the agent works on a prompt.

use genai::chat::{ToolCall, ToolResponse, Usage};

/// Progress reported by [`Agent::prompt`](crate::Agent::prompt).
///
/// Events borrow from the agent's history to avoid copies; clone what you
/// need to keep beyond the callback.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// The agent is asking the model for its next turn.
    TurnStart,
    /// A piece of the model's reasoning, as it arrives, from models that
    /// think before they answer.
    ReasoningDelta(&'a str),
    /// A piece of the text the model is writing, as it arrives.
    TextDelta(&'a str),
    /// The whole text the model wrote in a turn, once the turn is done.
    Text(&'a str),
    /// The model finished its turn, with the tokens it used when the
    /// provider reports them. Not sent for a turn cut short.
    TurnEnd(Option<&'a Usage>),
    /// A tool call the model made, before it runs.
    ToolCall(&'a ToolCall),
    /// The result of a tool call, as sent back to the model.
    ToolResult(&'a ToolResponse),
}
