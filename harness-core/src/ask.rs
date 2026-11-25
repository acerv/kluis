// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The `ask_user` tool: the model asks the user a question mid-task.
//!
//! It runs on the host, not in the sandbox: the agent sends a [`Question`]
//! through an [`Asker`] and waits for the UI to answer it.

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::tools::ToolError;

/// Name of the tool.
pub const ASK_TOOL: &str = "ask_user";

/// Maximum number of answer options of a question.
pub const MAX_ASK_OPTIONS: usize = 10;

/// Where the agent sends the questions of the model.
pub type Asker = mpsc::UnboundedSender<Question>;

/// A call of the `ask_user` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskTool {
    pub question: String,
    /// Suggested answers; the user may still answer something else.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOutput {
    pub answer: String,
}

/// A question waiting for the user. Dropping it leaves it unanswered.
#[derive(Debug)]
pub struct Question {
    pub text: String,
    pub options: Vec<String>,
    reply: oneshot::Sender<String>,
}

impl Question {
    /// The question `text` with the answers `options` suggested, and where
    /// its answer comes back.
    pub(crate) fn new(text: String, options: Vec<String>) -> (Self, oneshot::Receiver<String>) {
        let (reply, answer) = oneshot::channel();
        let question = Self {
            text,
            options,
            reply,
        };
        (question, answer)
    }

    /// Send the user's answer back to the model.
    pub fn answer(self, answer: impl Into<String>) {
        // The agent may have stopped waiting, e.g. when cancelled.
        let _ = self.reply.send(answer.into());
    }
}

impl AskTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "options": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["question"]
        })
    }

    /// The `ask_user` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new(ASK_TOOL)
            .with_description(
                "Ask the user a question and wait for the answer. Optionally \
                 suggest answers in options.",
            )
            .with_schema(Self::schema())
    }

    /// Send the question through `asker` and wait for the answer.
    ///
    /// The question must not be blank, and there can be at most
    /// [`MAX_ASK_OPTIONS`] options, none of them blank.
    pub async fn run(self, asker: Option<&Asker>) -> Result<AskOutput, ToolError> {
        if self.question.trim().is_empty() {
            return Err(ToolError::new("question must not be empty"));
        }
        if self.options.len() > MAX_ASK_OPTIONS {
            return Err(ToolError::new(format!(
                "at most {MAX_ASK_OPTIONS} options are allowed"
            )));
        }
        if self.options.iter().any(|o| o.trim().is_empty()) {
            return Err(ToolError::new("options must not be empty"));
        }
        let no_user = || ToolError::new("no user available to answer");
        let asker = asker.ok_or_else(no_user)?;
        let (question, answer) = Question::new(self.question, self.options);
        asker.send(question).map_err(|_| no_user())?;
        let answer = answer
            .await
            .map_err(|_| ToolError::new("the user did not answer"))?;
        Ok(AskOutput { answer })
    }
}
