// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The agent loop.

use std::fmt;
use std::str::FromStr;

use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart,
    MessageContent, ReasoningEffort, StopReason, ToolCall, ToolResponse, Usage,
};
use genai::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::models::{discover_models, ModelList};
use crate::oauth::OAuthLogins;
use crate::tools::{
    self, error_response, AskTool, Asker, Executor, FetchTool, HostTools, ToolContext,
};
use crate::Event;

/// Error returned by the agent.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Thinking / reasoning effort sent to models that support it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    /// Leave the provider's default reasoning behavior untouched.
    #[default]
    Default,
    Low,
    Medium,
    High,
}

impl Thinking {
    /// Lowercase effort name (`"default"`, `"low"`, `"medium"`, `"high"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Next effort level in the cycle `default -> low -> medium -> high -> default`.
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Default,
        }
    }

    fn reasoning_effort(self) -> Option<ReasoningEffort> {
        match self {
            Self::Default => None,
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
        }
    }
}

impl fmt::Display for Thinking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Thinking {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("default") {
            Ok(Self::Default)
        } else if trimmed.eq_ignore_ascii_case("low") {
            Ok(Self::Low)
        } else if trimmed.eq_ignore_ascii_case("medium") {
            Ok(Self::Medium)
        } else if trimmed.eq_ignore_ascii_case("high") {
            Ok(Self::High)
        } else {
            Err(format!(
                "invalid thinking effort '{s}' (expected default, low, medium, or high)"
            ))
        }
    }
}

/// A conversation with a model that can use the tools.
///
/// The history is kept across [`Agent::prompt`] calls.
pub struct Agent {
    client: Client,
    oauth: Option<OAuthLogins>,
    model: String,
    thinking: Thinking,
    req: ChatRequest,
    executor: Executor,
    host: HostTools,
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
            oauth: None,
            model: model.into(),
            thinking: Thinking::Default,
            req: ChatRequest::default().with_tools(tools::definitions()),
            executor: Executor::default(),
            host: HostTools::default(),
        }
    }

    /// Shape requests and list models of the adapters covered by `oauth` as
    /// their login requires.
    pub fn with_oauth(mut self, oauth: Option<OAuthLogins>) -> Self {
        self.oauth = oauth;
        self
    }

    /// Set the thinking effort sent with requests.
    pub fn with_thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = thinking;
        self
    }

    /// The thinking effort currently configured.
    pub fn thinking(&self) -> Thinking {
        self.thinking
    }

    /// Switch the thinking effort used for subsequent prompts.
    pub fn set_thinking(&mut self, thinking: Thinking) {
        self.thinking = thinking;
    }

    /// Set the tool executor used to run model tool calls.
    pub fn with_executor(mut self, executor: Executor) -> Self {
        self.executor = executor;
        self
    }

    /// Offer the `ask_user` tool, sending its questions through `asker`.
    pub fn with_asker(mut self, asker: Asker) -> Self {
        if self.host.asker.replace(asker).is_none() {
            self.req = self.req.append_tool(AskTool::definition());
        }
        self
    }

    /// Offer the `web_fetch` tool, which reaches the network from the host.
    pub fn with_web_fetch(mut self) -> Result<Self, Error> {
        if self.host.fetcher.is_none() {
            self.host.fetcher = Some(FetchTool::client()?);
            self.req = self.req.append_tool(FetchTool::definition());
        }
        Ok(self)
    }

    /// Set how local tools run (shorthand for `with_executor(Executor::Local(tools))`).
    pub fn with_tool_context(mut self, tools: ToolContext) -> Self {
        self.executor = Executor::Local(tools);
        self
    }

    /// Set the system prompt sent with every request. An empty string clears
    /// it.
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        let system = system.into();
        self.req.system = (!system.is_empty()).then_some(system);
        self
    }

    /// The system prompt currently configured for requests, if any.
    pub fn system(&self) -> Option<&str> {
        self.req.system.as_deref()
    }

    /// The model name currently used for requests.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Switch the model used for subsequent prompts.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    /// Query `adapters` concurrently via this agent's HTTP client for available
    /// chat models.
    pub async fn list_models(&self, adapters: &[AdapterKind]) -> ModelList {
        discover_models(&self.client, self.oauth.as_ref(), adapters).await
    }

    /// Drop the conversation history, keeping the model, system prompt, and
    /// tool context.
    pub fn clear(&mut self) {
        self.req.messages.clear();
    }

    /// Replace the conversation history, keeping the model, system prompt, and
    /// tool context.
    pub fn set_history(&mut self, messages: Vec<ChatMessage>) {
        self.req.messages = messages;
    }

    /// Initialize the agent with an existing conversation history.
    pub fn with_history(mut self, messages: Vec<ChatMessage>) -> Self {
        self.req.messages = messages;
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

        loop {
            on_event(Event::TurnStart);
            let turn = self.next_turn(&mut on_event).await?;

            // Keep the whole assistant turn (text, tool calls, thought
            // signatures) in the history, before any tool result.
            self.req.messages.push(ChatMessage::assistant(turn.content));
            let content = &self.req.messages[self.req.messages.len() - 1].content;

            let answer = content.first_text().unwrap_or_default();
            if !answer.is_empty() {
                on_event(Event::Text(answer));
            }
            on_event(Event::TurnEnd(turn.usage.as_ref()));

            // A copy, so results can join the history while calls run.
            let calls: Vec<ToolCall> = content.tool_calls().into_iter().cloned().collect();
            if calls.is_empty() {
                return Ok(answer.to_owned());
            }

            let mut pending = Pending {
                messages: &mut self.req.messages,
                calls: &calls,
                answered: 0,
            };
            let mut idx = 0;
            while idx < calls.len() {
                // Batch consecutive read-only calls (`read`, `ls`, `find`, `grep`)
                // and run them concurrently while emitting paired events and
                // tool responses in original call order.
                let batch_end = if turn.truncated {
                    idx + 1
                } else {
                    let mut end = idx;
                    while end < calls.len() && tools::is_read_only(&calls[end].fn_name) {
                        end += 1;
                    }
                    end.max(idx + 1)
                };
                let batch = &calls[idx..batch_end];
                if batch.len() == 1 {
                    let call = &batch[0];
                    on_event(Event::ToolCall(call));
                    let (resp, details) = if turn.truncated {
                        (error_response(call, TRUNCATED), None)
                    } else if let Some(resp) = self.host.run(call).await {
                        (resp, None)
                    } else {
                        self.executor
                            .execute(call, |chunk| on_event(Event::ToolOutput(chunk)))
                            .await
                    };
                    on_event(Event::ToolResult(&resp, details.as_ref()));
                    pending.answer(resp);
                } else {
                    let results = futures_util::future::join_all(
                        batch.iter().map(|call| self.executor.execute(call, |_| {})),
                    )
                    .await;
                    for (call, (resp, details)) in batch.iter().zip(results) {
                        on_event(Event::ToolCall(call));
                        on_event(Event::ToolResult(&resp, details.as_ref()));
                        pending.answer(resp);
                    }
                }
                idx = batch_end;
            }
        }
    }

    /// Text the login used for the model requires as the first system
    /// prompt block.
    fn system_prefix(&self) -> Option<&'static str> {
        let kind = AdapterKind::from_model(&self.model).ok()?;
        self.oauth.as_ref()?.system_prefix(kind)
    }

    /// Ask the model for its next turn, reporting its text as it streams,
    /// and return it with its token usage.
    ///
    /// The turn joins the history only once complete: a cut stream, like
    /// pi does, is never sent back to the model.
    async fn next_turn(&self, on_event: &mut impl FnMut(Event<'_>)) -> Result<Turn, Error> {
        let mut options = ChatOptions::default()
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_reasoning_content(true)
            .with_capture_usage(true);
        if let Some(effort) = self.thinking.reasoning_effort() {
            options = options.with_reasoning_effort(effort);
        }
        let mut req = self.req.clone();
        if let Some(prefix) = self.system_prefix() {
            // Keep our prompt as a separate block: with a cache breakpoint,
            // genai sends system blocks apart instead of joining them.
            if let Some(system) = req.system.replace(prefix.to_owned()) {
                req.messages.insert(0, ChatMessage::system(system));
            }
            options = options.with_cache_control(CacheControl::Ephemeral);
        }
        let res = self
            .client
            .exec_chat_stream(&self.model, req, Some(&options))
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
                    let truncated = end
                        .captured_stop_reason
                        .as_ref()
                        .is_some_and(StopReason::is_max_tokens);
                    if truncated {
                        // Replace incomplete/unparsed arguments with `{}` so
                        // providers requiring an object accept the next turn.
                        for part in content.iter_mut() {
                            if let ContentPart::ToolCall(call) = part {
                                if !call.fn_arguments.is_object() {
                                    call.fn_arguments = json!({});
                                }
                            }
                        }
                    }
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
                    return Ok(Turn {
                        content,
                        usage: end.captured_usage,
                        truncated,
                    });
                }
                _ => {}
            }
        }
        Err("the model's reply ended early".into())
    }
}

struct Turn {
    content: MessageContent,
    usage: Option<Usage>,
    truncated: bool,
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

/// Result of a tool call from a response truncated by the token limit.
const TRUNCATED: &str = "response was truncated at max_tokens; tool call was not executed — please re-issue it (split into smaller calls if needed)";

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
            self.messages.push(error_response(call, CANCELLED).into());
        }
    }
}
