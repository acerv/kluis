// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What happens while the agent works on a prompt.

use genai::chat::{ToolCall, ToolResponse};

/// Progress reported by [`Agent::prompt`](crate::Agent::prompt).
///
/// Events borrow from the agent's history to avoid copies; clone what you
/// need to keep beyond the callback.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// Text the model wrote in a turn.
    Text(&'a str),
    /// A tool call the model made, before it runs.
    ToolCall(&'a ToolCall),
    /// The result of a tool call, as sent back to the model.
    ToolResult(&'a ToolResponse),
}
