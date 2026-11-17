// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The agent loop.

use genai::chat::{ChatMessage, ChatRequest, ToolCall};
use genai::Client;

use crate::tools;
use crate::Event;

/// Error returned by the agent.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Default maximum number of model calls for a single prompt.
pub const DEFAULT_MAX_TURNS: usize = 10;

/// A conversation with a model that can use the tools.
///
/// The history is kept across [`Agent::prompt`] calls.
pub struct Agent {
    client: Client,
    model: String,
    req: ChatRequest,
    max_turns: usize,
}

impl Agent {
    /// Create an agent for `model`; genai picks the provider from its name.
    pub fn new(model: impl Into<String>) -> Result<Self, Error> {
        Ok(Self {
            client: Client::new()?,
            model: model.into(),
            req: ChatRequest::default().with_tools(tools::definitions()),
            max_turns: DEFAULT_MAX_TURNS,
        })
    }

    /// Set the maximum number of model calls for a single prompt.
    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Send a user message, run the tool calls the model makes, and return
    /// the model's final answer.
    ///
    /// `on_event` is called as the work progresses.
    pub async fn prompt(
        &mut self,
        text: &str,
        mut on_event: impl FnMut(Event),
    ) -> Result<String, Error> {
        self.req.messages.push(ChatMessage::user(text));

        for _ in 0..self.max_turns {
            let res = self
                .client
                .exec_chat(&self.model, self.req.clone(), None)
                .await?;
            let answer = res.first_text().unwrap_or_default().to_owned();
            let calls: Vec<ToolCall> = res.tool_calls().into_iter().cloned().collect();

            if !answer.is_empty() {
                on_event(Event::Text(answer.clone()));
            }

            // Keep the whole assistant turn (text, tool calls, thought
            // signatures) in the history, before any tool result.
            self.req.messages.push(ChatMessage::assistant(res.content));
            if calls.is_empty() {
                return Ok(answer);
            }

            for call in calls {
                on_event(Event::ToolCall(call.clone()));
                let resp = tools::run(&call).await;
                on_event(Event::ToolResult(resp.clone()));
                self.req.messages.push(resp.into());
            }
        }

        Err(format!("stopped after {} turns", self.max_turns).into())
    }
}
