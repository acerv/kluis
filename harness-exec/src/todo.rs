// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;

/// Maximum number of items in a todo list.
pub const MAX_TODO_ITEMS: usize = 100;

/// Maximum bytes of the text of one todo item.
pub const MAX_TODO_ITEM_BYTES: usize = 1024;

/// A call of the `todo` tool: the whole new list, replacing the old one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoTool {
    pub todos: Vec<Todo>,
}

/// One task of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    /// The `todo` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new("todo")
            .with_description(
                "Track the plan of a multi-step task. Send the whole list each \
                 time; keep one item in_progress at a time.",
            )
            .with_schema(Self::schema())
    }

    /// Check the list and return it, so the plan stays in view.
    ///
    /// Items must have non-blank text of at most [`MAX_TODO_ITEM_BYTES`],
    /// the list at most [`MAX_TODO_ITEMS`] items and at most one of them in
    /// progress. An empty list clears the plan.
    pub fn run(self) -> Result<Self, ToolError> {
        if self.todos.len() > MAX_TODO_ITEMS {
            return Err(ToolError::new(format!(
                "at most {MAX_TODO_ITEMS} todos are allowed"
            )));
        }
        let mut in_progress = false;
        for todo in &self.todos {
            if todo.content.trim().is_empty() {
                return Err(ToolError::new("todo content must not be empty"));
            }
            if todo.content.len() > MAX_TODO_ITEM_BYTES {
                return Err(ToolError::new(format!(
                    "todo content must be at most {MAX_TODO_ITEM_BYTES} bytes"
                )));
            }
            if todo.status == TodoStatus::InProgress {
                if in_progress {
                    return Err(ToolError::new("only one todo can be in_progress"));
                }
                in_progress = true;
            }
        }
        Ok(self)
    }
}
