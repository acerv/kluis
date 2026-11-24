// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error returned by a tool. Meant to be sent back to the model as text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{message}")]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn io(path: &str, err: std::io::Error) -> Self {
        Self::new(format!("{path}: {err}"))
    }
}

/// What a tool that failed with `message` answers: `{"error": "..."}`, so
/// the model can recover.
pub fn error_json(message: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": message.to_string() }).to_string()
}
