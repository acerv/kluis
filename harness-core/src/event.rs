// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What happens while the agent works on a prompt.

use genai::chat::{ToolCall, ToolResponse};

/// Progress reported by [`Agent::prompt`](crate::Agent::prompt).
#[derive(Debug, Clone)]
pub enum Event {
    /// Text the model wrote in a turn.
    Text(String),
    /// A tool call the model made, before it runs.
    ToolCall(ToolCall),
    /// The result of a tool call, as sent back to the model.
    ToolResult(ToolResponse),
}
