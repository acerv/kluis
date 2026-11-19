// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The agent loop.

use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent, ToolCall,
    ToolResponse, Usage,
};
use genai::Client;
use serde_json::json;

use crate::tools::{self, ToolContext};
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
    tools: ToolContext,
}

impl Agent {
    /// Create an agent for `model`; genai picks the provider from its name.
    pub fn new(model: impl Into<String>) -> Result<Self, Error> {
        Ok(Self::with_client(Client::new()?, model))
    }

    /// Create an agent for `model` using `client`, e.g. one sending requests
    /// to a custom endpoint.
    pub fn with_client(client: Client, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            req: ChatRequest::default().with_tools(tools::definitions()),
            max_turns: DEFAULT_MAX_TURNS,
            tools: ToolContext::default(),
        }
    }

    /// Set the maximum number of model calls for a single prompt.
    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Set how tools run, e.g. a sandbox for commands.
    pub fn with_tool_context(mut self, tools: ToolContext) -> Self {
        self.tools = tools;
        self
    }

    /// The conversation so far: user messages, model turns and tool
    /// results, oldest first.
    pub fn history(&self) -> &[ChatMessage] {
        &self.req.messages
    }

    /// Send a user message, run the tool calls the model makes, and return
    /// the model's final answer.
    ///
    /// `on_event` is called as the work progresses.
    ///
    /// Dropping the future, e.g. to cancel, keeps the history valid for the
    /// next prompt: the user message stays, a reply cut while streaming is
    /// left out, tool results already made stay, and the calls left get a
    /// result saying they were cancelled.
    pub async fn prompt(
        &mut self,
        text: &str,
        mut on_event: impl FnMut(Event<'_>),
    ) -> Result<String, Error> {
        self.req.messages.push(ChatMessage::user(text));

        for _ in 0..self.max_turns {
            on_event(Event::TurnStart);
            let (content, usage) = self.next_turn(&mut on_event).await?;

            // Keep the whole assistant turn (text, tool calls, thought
            // signatures) in the history, before any tool result.
            self.req.messages.push(ChatMessage::assistant(content));
            let turn = &self.req.messages[self.req.messages.len() - 1].content;

            let answer = turn.first_text().unwrap_or_default();
            if !answer.is_empty() {
                on_event(Event::Text(answer));
            }
            on_event(Event::TurnEnd(usage.as_ref()));

            // A copy, so results can join the history while calls run.
            let calls: Vec<ToolCall> = turn.tool_calls().into_iter().cloned().collect();
            if calls.is_empty() {
                return Ok(answer.to_owned());
            }

            let mut pending = Pending {
                messages: &mut self.req.messages,
                calls: &calls,
                answered: 0,
            };
            for call in &calls {
                on_event(Event::ToolCall(call));
                let (resp, details) = tools::execute(call, &self.tools).await;
                on_event(Event::ToolResult(&resp, details.as_ref()));
                pending.answer(resp);
            }
        }

        Err(format!("stopped after {} turns", self.max_turns).into())
    }

    /// Ask the model for its next turn, reporting its text as it streams,
    /// and return it with its token usage.
    ///
    /// The turn joins the history only once complete: a cut stream, like
    /// pi does, is never sent back to the model.
    async fn next_turn(
        &self,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> Result<(MessageContent, Option<Usage>), Error> {
        let options = ChatOptions::default()
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_reasoning_content(true)
            .with_capture_usage(true);
        let res = self
            .client
            .exec_chat_stream(&self.model, self.req.clone(), Some(&options))
            .await?;
        let reasoning_back = needs_reasoning_back(res.model_iden.adapter_kind);
        let mut stream = res.stream;
        while let Some(event) = stream.next().await {
            match event? {
                ChatStreamEvent::Chunk(chunk) => on_event(Event::TextDelta(&chunk.content)),
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    on_event(Event::ReasoningDelta(&chunk.content))
                }
                ChatStreamEvent::End(end) => {
                    let mut content = end.captured_content.unwrap_or_default();
                    // Unless the turn already carries its reasoning, e.g.
                    // signed, which an extra part would break.
                    let carried = content
                        .parts()
                        .iter()
                        .any(|p| p.is_reasoning_content() || p.is_thought_signature());
                    if let Some(reasoning) = end.captured_reasoning_content {
                        if reasoning_back && !carried && !reasoning.is_empty() {
                            content.prepend(ContentPart::ReasoningContent(reasoning));
                        }
                    }
                    return Ok((content, end.captured_usage));
                }
                _ => {}
            }
        }
        Err("the model's reply ended early".into())
    }
}

/// Whether the provider wants the reasoning of a turn back in the next
/// requests, as DeepSeek and Kimi do for tool calls in thinking mode.
/// Others may refuse it, so it stays out.
fn needs_reasoning_back(kind: AdapterKind) -> bool {
    matches!(
        kind,
        AdapterKind::DeepSeek | AdapterKind::Kimi | AdapterKind::Moonshot
    )
}

/// Result of a tool call the agent stopped waiting for.
const CANCELLED: &str = "cancelled; the call may have partly run";

/// The tool calls of a turn, answered in order. Dropping it answers the
/// ones left as cancelled, so the history never holds a call without its
/// result, which providers reject.
struct Pending<'a> {
    messages: &'a mut Vec<ChatMessage>,
    calls: &'a [ToolCall],
    answered: usize,
}

impl Pending<'_> {
    fn answer(&mut self, resp: ToolResponse) {
        self.messages.push(resp.into());
        self.answered += 1;
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        for call in &self.calls[self.answered..] {
            let content = json!({ "error": CANCELLED }).to_string();
            self.messages
                .push(ToolResponse::from_tool_call(call, content).into());
        }
    }
}
