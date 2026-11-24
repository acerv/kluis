// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The `task` tool: the model hands a part of its work to a subagent.
//!
//! The agent runs it itself: a subagent is another conversation with the
//! same model and tools, whose answer is all the model gets back.

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Name of the tool.
pub const TASK_TOOL: &str = "task";

/// A call of the `task` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTool {
    /// What the subagent is asked, which is all it knows.
    pub prompt: String,
    /// Whether the subagent only gets the tools that change nothing.
    /// Such tasks asked together run at the same time.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutput {
    pub answer: String,
}

impl TaskTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string" },
                "read_only": {
                    "type": "boolean",
                    "description": "Only read, search and fetch, changing nothing. \
                        Read-only tasks asked together run at the same time."
                }
            },
            "required": ["prompt"]
        })
    }

    /// The `task` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new(TASK_TOOL)
            .with_description(
                "Give a task to a subagent and get its answer. It has the same \
                 tools but none of this conversation: say all it needs in prompt. \
                 For a search or a question, set read_only and ask several \
                 tasks at once to run them in parallel.",
            )
            .with_schema(Self::schema())
    }
}
