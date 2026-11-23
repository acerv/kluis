// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The agent loop.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    Binary, CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatRole, ChatStreamEvent,
    ContentPart, MessageContent, ReasoningEffort, StopReason, ToolCall, ToolResponse, Usage,
};
use genai::{Client, ModelIden};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::models::{discover_models, ModelList};
use crate::oauth::OAuthLogins;
use crate::retry::{is_too_large, EndedEarly, RetryPolicy};
use crate::session::TokenUsage;
use crate::tools::{
    self, error_response, AskTool, Asker, Executor, FetchTool, HostTools, Image, ToolContext,
    ToolDetails,
};
use crate::Event;

/// Error returned by the agent.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Default maximum model turns executed in a single [`Agent::prompt`] call.
pub const DEFAULT_MAX_TURNS: usize = 50;

/// Default prompt-token threshold at which [`Agent::prompt`] automatically
/// compacts the conversation while the prompt cache is still warm.
pub const DEFAULT_AUTO_COMPACT_TOKENS: u64 = 100_000;

/// Consecutive identical turns (or identical calls within one turn) before the
/// agent refuses to re-run them and asks the model to change approach.
const MAX_REPEATED_CALLS: usize = 3;

/// Thinking / reasoning effort sent to models that support it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    /// Leave the provider's default reasoning behavior untouched.
    #[default]
    Default,
    /// Disable reasoning where the provider supports turning it off.
    Off,
    Low,
    Medium,
    High,
}

impl Thinking {
    const ALL: [Self; 5] = [
        Self::Default,
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
    ];

    /// Lowercase effort name (`"default"`, `"off"`, `"low"`, `"medium"`, `"high"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Next effort level in the cycle `default -> off -> low -> medium -> high -> default`.
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Off,
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Default,
        }
    }

    fn reasoning_effort(self) -> Option<ReasoningEffort> {
        match self {
            Self::Default => None,
            Self::Off => Some(ReasoningEffort::Zero),
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
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(trimmed))
            .ok_or_else(|| {
                format!(
                    "invalid thinking effort '{s}' (expected default, off, low, medium, or high)"
                )
            })
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
    cache_key: Uuid,
    executor: Executor,
    host: HostTools,
    retry: RetryPolicy,
    max_turns: Option<usize>,
    auto_compact_tokens: Option<u64>,
    steering: Steering,
}

/// Messages of the user waiting to join a running prompt, shared with the
/// UI that queues them: see [`Agent::steering`].
#[derive(Debug, Clone, Default)]
pub struct Steering(Arc<Mutex<Vec<String>>>);

impl Steering {
    /// Queue `text` for the running prompt.
    pub fn push(&self, text: impl Into<String>) {
        self.queue().push(text.into());
    }

    /// Take the queued messages, oldest first.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.queue())
    }

    fn is_empty(&self) -> bool {
        self.queue().is_empty()
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        // The queue stays whole even if a thread panicked holding it.
        self.0.lock().unwrap_or_else(|err| err.into_inner())
    }
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
            cache_key: Uuid::new_v4(),
            executor: Executor::default(),
            host: HostTools::default(),
            retry: RetryPolicy::default(),
            max_turns: Some(DEFAULT_MAX_TURNS),
            auto_compact_tokens: Some(DEFAULT_AUTO_COMPACT_TOKENS),
            steering: Steering::default(),
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

    /// Set when failed model requests are retried.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Limit how many model turns a single [`Agent::prompt`] call may run, or
    /// pass `None` for no limit. Defaults to `Some(DEFAULT_MAX_TURNS)`.
    pub fn with_max_turns(mut self, max_turns: Option<usize>) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Maximum model turns per [`Agent::prompt`] call, or `None` when unlimited.
    pub fn max_turns(&self) -> Option<usize> {
        self.max_turns
    }

    /// Automatically compact the conversation when a turn's prompt tokens reach
    /// `tokens`, or pass `None` / `Some(0)` to disable auto-compaction.
    /// Defaults to `Some(DEFAULT_AUTO_COMPACT_TOKENS)`.
    pub fn with_auto_compact(mut self, tokens: Option<u64>) -> Self {
        self.auto_compact_tokens = tokens.filter(|&n| n > 0);
        self
    }

    /// Prompt-token threshold that triggers auto-compaction, or `None` when disabled.
    pub fn auto_compact(&self) -> Option<u64> {
        self.auto_compact_tokens
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
        self.req.messages = Vec::new();
        self.cache_key = Uuid::new_v4();
    }

    /// Replace the conversation history, keeping the model, system prompt, and
    /// tool context.
    pub fn set_history(&mut self, messages: Vec<ChatMessage>) {
        self.req.messages = messages;
        self.cache_key = Uuid::new_v4();
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

    /// A handle to queue user messages while a prompt runs: they join the
    /// conversation before the next model turn, after the results of the
    /// tool calls running. A prompt whose model answered goes on with them
    /// rather than ending; the ones queued once it ended stay queued.
    pub fn steering(&self) -> Steering {
        self.steering.clone()
    }

    /// Run the shell `command` for the user as the `bash` tool runs it for
    /// the model, in the sandbox when the tools run there, and return its
    /// result. Nothing joins the conversation.
    pub async fn run_command(&self, command: &str, on_output: impl FnMut(&str)) -> String {
        let call = ToolCall {
            call_id: "user".to_owned(),
            fn_name: "bash".to_owned(),
            fn_arguments: json!({ "command": command }),
            thought_signatures: None,
        };
        self.executor.execute(&call, on_output).await.0.content
    }

    /// Stop what tool commands left running in the background, when they
    /// run in the sandbox: call it between prompts.
    pub async fn stop_background(&self) {
        self.executor.stop_background().await;
    }

    /// Send a user message, run the tool calls the model makes, and return
    /// the model's final answer.
    ///
    /// `on_event` is called as the work progresses.
    ///
    /// A turn without text nor tool calls is an error, and stays out of the
    /// history: providers refuse it when sent back.
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
        // Where the messages not yet reported with `Event::Messages` start.
        let mut saved = self.req.messages.len();
        self.req.messages.push(ChatMessage::user(text));

        let mut turns = 0usize;
        let mut prev_assistant_idx: Option<usize> = None;
        let mut repeat_streak = 0usize;
        let mut compact_blocked = false;

        loop {
            if let Some(limit) = self.max_turns {
                if turns >= limit {
                    return Err(format!("turn limit reached ({limit} turns)").into());
                }
            }
            turns += 1;

            for text in self.steering.take() {
                on_event(Event::Steered(&text));
                self.req.messages.push(ChatMessage::user(text));
            }
            if saved < self.req.messages.len() {
                on_event(Event::Messages(&self.req.messages[saved..]));
                saved = self.req.messages.len();
            }
            on_event(Event::TurnStart);
            let turn = match self.next_turn_retrying(None, &mut on_event).await {
                Ok(turn) => turn,
                // Tool outputs are most of a long conversation: drop the
                // old ones and ask again, until none is left to drop.
                Err(err) if is_too_large(err.as_ref()) => {
                    match prune_tool_outputs(&mut self.req.messages) {
                        0 => return Err(format!("{err} (start a new conversation)").into()),
                        pruned => {
                            on_event(Event::Pruned(pruned));
                            turns -= 1;
                            continue;
                        }
                    }
                }
                Err(err) => return Err(err),
            };
            if !turn.content.contains_text() && !turn.content.contains_tool_call() {
                // Kept out of the history: providers refuse an empty turn.
                on_event(Event::TurnEnd(turn.usage.as_ref()));
                return Err(if turn.truncated {
                    "the reply was cut at the token limit before any text"
                } else {
                    "the model returned an empty reply"
                }
                .into());
            }

            // Keep the whole assistant turn (text, tool calls, thought
            // signatures) in the history, before any tool result.
            let assistant_idx = self.req.messages.len();
            self.req.messages.push(ChatMessage::assistant(turn.content));
            let content = &self.req.messages[assistant_idx].content;

            let answer = content.first_text().unwrap_or_default();
            if !answer.is_empty() {
                on_event(Event::Text(answer));
            }
            on_event(Event::TurnEnd(turn.usage.as_ref()));

            let context = turn
                .usage
                .as_ref()
                .map_or(0, |u| TokenUsage::from_genai(u).prompt());
            let need_compact = match self.auto_compact_tokens {
                Some(limit) if context >= limit => !compact_blocked,
                Some(_) if context > 0 => {
                    compact_blocked = false;
                    false
                }
                _ => false,
            };

            // A copy, so results can join the history while calls run.
            let calls: Vec<ToolCall> = content.tool_calls().into_iter().cloned().collect();
            if calls.is_empty() {
                if turn.truncated {
                    on_event(Event::Truncated);
                }
                // Messages queued meanwhile go on with the prompt.
                if self.steering.is_empty() {
                    let answer = answer.to_owned();
                    if need_compact {
                        self.run_auto_compact(context, false, &mut on_event).await;
                    }
                    return Ok(answer);
                }
            }

            let same_as_prev = !turn.truncated
                && prev_assistant_idx.is_some_and(|prev| {
                    same_calls(&self.req.messages[prev].content.tool_calls(), &calls)
                });
            repeat_streak = if turn.truncated {
                0
            } else if same_as_prev {
                repeat_streak + 1
            } else {
                1
            };
            prev_assistant_idx = (!turn.truncated).then_some(assistant_idx);
            let repeated_turn = repeat_streak >= MAX_REPEATED_CALLS;

            // Why the call at `idx` must not run, if so.
            let refusal = |idx: usize, call: &ToolCall| {
                if turn.truncated {
                    Some(TRUNCATED)
                } else if turn.invalid.contains(&call.call_id) {
                    Some(INVALID_ARGUMENTS)
                } else if repeated_turn || is_within_turn_repeat(&calls[..idx], call) {
                    Some(REPEATED_CALL)
                } else {
                    None
                }
            };
            let mut pending = Pending {
                messages: &mut self.req.messages,
                calls: &calls,
                answered: 0,
            };
            // Images the tools read, sent after all the results: a message
            // between them would split the turn.
            let mut images = Vec::new();
            let mut idx = 0;
            while idx < calls.len() {
                // Batch consecutive read-only calls (`read`, `ls`, `find`, `grep`)
                // and run them concurrently while emitting paired events and
                // tool responses in original call order.
                let batch_end = if turn.truncated || repeated_turn {
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
                    let (resp, details) = if let Some(why) = refusal(idx, call) {
                        (error_response(call, why), None)
                    } else if let Some(resp) = self.host.run(call).await {
                        (resp, None)
                    } else {
                        self.executor
                            .execute(call, |chunk| on_event(Event::ToolOutput(chunk)))
                            .await
                    };
                    let details = take_image(details, &mut images);
                    on_event(Event::ToolResult(&resp, details.as_ref()));
                    pending.answer(resp);
                } else {
                    let executor = &self.executor;
                    let results = futures_util::future::join_all(batch.iter().enumerate().map(
                        |(off, call)| {
                            let refusal = refusal(idx + off, call);
                            async move {
                                if let Some(why) = refusal {
                                    (error_response(call, why), None)
                                } else {
                                    executor.execute(call, |_| {}).await
                                }
                            }
                        },
                    ))
                    .await;
                    for (call, (resp, details)) in batch.iter().zip(results) {
                        let details = take_image(details, &mut images);
                        on_event(Event::ToolCall(call));
                        on_event(Event::ToolResult(&resp, details.as_ref()));
                        pending.answer(resp);
                    }
                }
                idx = batch_end;
            }
            drop(pending);
            drop(calls);
            if !images.is_empty() {
                images.insert(0, ContentPart::Text(IMAGES.to_owned()));
                self.req
                    .messages
                    .push(ChatMessage::user(MessageContent::from_parts(images)));
            }

            if need_compact && self.max_turns.is_none_or(|limit| turns < limit) {
                if self.run_auto_compact(context, true, &mut on_event).await {
                    saved = self.req.messages.len();
                    prev_assistant_idx = None;
                    repeat_streak = 0;
                }
                compact_blocked = true;
            }
        }
    }

    /// Ask the model to summarize the conversation so far and replace the
    /// history with a compact handoff, freeing context tokens for subsequent
    /// prompts.
    ///
    /// An optional `focus` steers what the summary should preserve. If the
    /// request fails, is cancelled, or returns empty text, the existing
    /// history is left untouched.
    pub async fn compact(
        &mut self,
        focus: Option<&str>,
        mut on_event: impl FnMut(Event<'_>),
    ) -> Result<String, Error> {
        self.compact_inner(focus, false, &mut on_event).await
    }

    async fn run_auto_compact(
        &mut self,
        context_tokens: u64,
        continue_task: bool,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> bool {
        let before = self.req.messages.len();
        on_event(Event::Compacting { context_tokens });
        match self.compact_inner(None, continue_task, on_event).await {
            Ok(_) => {
                on_event(Event::Compacted {
                    before,
                    messages: &self.req.messages,
                });
                true
            }
            Err(err) => {
                let msg = err.to_string();
                on_event(Event::CompactFailed(&msg));
                false
            }
        }
    }

    async fn compact_inner(
        &mut self,
        focus: Option<&str>,
        continue_task: bool,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> Result<String, Error> {
        if self.req.messages.is_empty() {
            return Err("nothing to compact".into());
        }
        let instruction = compact_instruction(focus);
        on_event(Event::TurnStart);
        let turn = self
            .next_turn_retrying(Some(&instruction), on_event)
            .await?;

        let mut raw = turn.content.joined_texts().unwrap_or_default();
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            on_event(Event::Text(trimmed));
        }
        on_event(Event::TurnEnd(turn.usage.as_ref()));

        if trimmed.is_empty() {
            return Err("model returned an empty summary".into());
        }
        let mut compacted = Vec::with_capacity(2 + usize::from(continue_task));
        compacted.push(ChatMessage::user(format!("{COMPACT_HEADER}\n\n{trimmed}")));
        compacted.push(ChatMessage::assistant(COMPACT_ACK));
        if continue_task {
            compacted.push(ChatMessage::user(COMPACT_CONTINUE));
        }
        self.set_history(compacted);
        let end = raw.trim_end().len();
        let start = raw.len() - raw.trim_start().len();
        raw.truncate(end);
        if start > 0 {
            raw.drain(..start);
        }
        Ok(raw)
    }

    /// Text the login used for `kind` requires as the first system prompt
    /// block.
    fn system_prefix(&self, kind: AdapterKind) -> Option<&'static str> {
        self.oauth.as_ref()?.system_prefix(kind)
    }

    /// Like [`next_turn`](Self::next_turn), retrying failures that may pass
    /// as the [`RetryPolicy`] says.
    async fn next_turn_retrying(
        &self,
        extra_user: Option<&str>,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> Result<Turn, Error> {
        let mut attempt = 0;
        loop {
            let err = match self.next_turn(extra_user, on_event).await {
                Ok(turn) => return Ok(turn),
                Err(err) => err,
            };
            attempt += 1;
            let Some((delay, reason)) = self.retry.delay(attempt, err.as_ref()) else {
                return Err(err);
            };
            on_event(Event::Retry {
                attempt,
                delay,
                reason: &reason,
            });
            tokio::time::sleep(delay).await;
        }
    }

    /// Build the per-turn request for `model`, applying any OAuth system
    /// prefix, an optional transient trailing user message, and Anthropic
    /// prompt-cache breakpoints without mutating `self.req` or shifting the
    /// cloned message buffer.
    fn build_request(
        &self,
        model: &ModelIden,
        prefix: Option<&'static str>,
        extra_user: Option<&str>,
    ) -> ChatRequest {
        let anthropic_cache = uses_anthropic_cache(model);
        let (system, leading_system) = match (prefix, self.req.system.as_deref()) {
            (Some(prefix), Some(sys)) => (Some(prefix.to_owned()), Some(ChatMessage::system(sys))),
            (Some(prefix), None) if anthropic_cache => (None, Some(ChatMessage::system(prefix))),
            (Some(prefix), None) => (Some(prefix.to_owned()), None),
            (None, Some(sys)) if anthropic_cache => (None, Some(ChatMessage::system(sys))),
            (None, sys) => (sys.map(str::to_owned), None),
        };
        let has_leading_system = leading_system.is_some();
        let leading_system = leading_system.map(|msg| {
            if anthropic_cache {
                msg.with_options(CacheControl::Ephemeral)
            } else {
                msg
            }
        });

        let mut messages = Vec::with_capacity(
            self.req.messages.len()
                + usize::from(has_leading_system)
                + usize::from(extra_user.is_some()),
        );
        if let Some(msg) = leading_system {
            messages.push(msg);
        }
        messages.extend(self.req.messages.iter().cloned());

        let mut tools = self.req.tools.clone();
        if anthropic_cache {
            if !has_leading_system {
                if let Some(last_tool) = tools.as_mut().and_then(|t| t.last_mut()) {
                    last_tool.cache_control = Some(CacheControl::Ephemeral);
                }
            }
            // Mark only persisted history so transient prompts (e.g. `/compact`)
            // hit the existing prefix cache without writing a discarded tail.
            mark_conversation_cache(&mut messages);
        }
        if let Some(text) = extra_user {
            messages.push(ChatMessage::user(text));
        }

        ChatRequest {
            system,
            messages,
            tools,
            ..ChatRequest::default()
        }
    }

    /// Ask the model for its next turn, reporting its text as it streams,
    /// and return it with its token usage.
    ///
    /// The turn joins the history only once complete: a cut stream, like
    /// pi does, is never sent back to the model.
    async fn next_turn(
        &self,
        extra_user: Option<&str>,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> Result<Turn, Error> {
        let target = self.client.resolve_service_target(&self.model).await?;
        let mut options = ChatOptions::default()
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_reasoning_content(true)
            .with_capture_usage(true);
        if let Some(effort) = self.thinking.reasoning_effort() {
            options = options.with_reasoning_effort(effort);
        }
        if uses_openai_cache_key(target.model.adapter_kind) {
            let mut buf = Uuid::encode_buffer();
            options =
                options.with_prompt_cache_key(&*self.cache_key.hyphenated().encode_lower(&mut buf));
        }
        let prefix = self.system_prefix(target.model.adapter_kind);
        let req = self.build_request(&target.model, prefix, extra_user);
        let res = self
            .client
            .exec_chat_stream(target, req, Some(&options))
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
                    let truncated = end
                        .captured_stop_reason
                        .as_ref()
                        .is_some_and(StopReason::is_max_tokens);
                    let mut parts = end.captured_content.unwrap_or_default().into_parts();
                    // Providers refuse blank text and arguments that are
                    // not an object when the turn is sent back.
                    parts.retain(
                        |part| !matches!(part, ContentPart::Text(t) if t.trim().is_empty()),
                    );
                    let mut invalid = Vec::new();
                    for part in &mut parts {
                        if let ContentPart::ToolCall(call) = part {
                            let args = &mut call.fn_arguments;
                            if !args.is_object() {
                                // No arguments at all is a call without any.
                                let none = args.is_null()
                                    || args.as_str().is_some_and(|s| s.trim().is_empty());
                                if !none {
                                    invalid.push(call.call_id.clone());
                                }
                                *args = json!({});
                            }
                        }
                    }
                    let mut content = MessageContent::from_parts(parts);
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
                        invalid,
                    });
                }
                _ => {}
            }
        }
        Err(EndedEarly.into())
    }
}

struct Turn {
    content: MessageContent,
    usage: Option<Usage>,
    truncated: bool,
    /// Ids of the tool calls whose arguments were not a JSON object.
    invalid: Vec<String>,
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

/// Whether `model` uses Anthropic's block-level `cache_control` protocol.
fn uses_anthropic_cache(model: &ModelIden) -> bool {
    let (_, name) = model.model_name.namespace_and_name();
    match model.adapter_kind {
        AdapterKind::Anthropic => true,
        AdapterKind::Vertex => name.starts_with("claude"),
        _ => false,
    }
}

/// Whether `kind` accepts OpenAI's `prompt_cache_key` routing hint.
fn uses_openai_cache_key(kind: AdapterKind) -> bool {
    matches!(kind, AdapterKind::OpenAI | AdapterKind::OpenAIResp)
}

/// Place rolling Anthropic prompt-cache breakpoints on `messages`:
///
/// 1. Last cacheable user/tool message so the request writes the current
///    prefix into the cache for the next turn.
/// 2. Last cacheable user/tool message before the final assistant turn,
///    matching the previous turn's write breakpoint even when the latest
///    turn added more than 20 content blocks.
fn mark_conversation_cache(messages: &mut [ChatMessage]) {
    mark_last_cacheable(messages);
    if let Some(last_assistant) = messages.iter().rposition(|m| m.role == ChatRole::Assistant) {
        mark_last_cacheable(&mut messages[..last_assistant]);
    }
}

fn mark_last_cacheable(messages: &mut [ChatMessage]) {
    if let Some(msg) = messages.iter_mut().rfind(|m| is_cacheable_message(m)) {
        msg.options
            .get_or_insert_with(Default::default)
            .cache_control = Some(CacheControl::Ephemeral);
    }
}

/// Whether `msg` is a user or tool turn with non-empty content that
/// Anthropic accepts a `cache_control` breakpoint on.
fn is_cacheable_message(msg: &ChatMessage) -> bool {
    match msg.role {
        ChatRole::User => msg.content.parts().iter().any(|part| match part {
            ContentPart::Text(text) => !text.is_empty(),
            ContentPart::Binary(_) | ContentPart::ToolResponse(_) | ContentPart::Custom(_) => true,
            _ => false,
        }),
        ChatRole::Tool => !msg.content.tool_responses().is_empty(),
        ChatRole::System | ChatRole::Assistant => false,
    }
}

/// Result of a tool call the agent stopped waiting for.
const CANCELLED: &str = "cancelled; the call may have partly run";

/// Result of a tool call from a response truncated by the token limit.
const TRUNCATED: &str = "response was truncated at max_tokens; tool call was not executed — please re-issue it (split into smaller calls if needed)";

/// Result of a tool call whose arguments could not be read.
const INVALID_ARGUMENTS: &str =
    "arguments were not a valid JSON object; tool call was not executed, please re-issue it";

/// Result of a tool call repeated identically too many times in a row.
const REPEATED_CALL: &str =
    "repeated identical tool call; try a different approach or explain the blocker";

/// Text before the images the tools read.
const IMAGES: &str = "The images read by the tools above, in order:";

/// Output of an old tool call removed to fit the context window.
const PRUNED: &str =
    r#"{"error":"output removed to fit the context window; run the tool again if needed"}"#;

/// An old image removed to fit the context window.
const PRUNED_IMAGE: &str = "[image removed to fit the context window]";

const COMPACT_PROMPT: &str = "Summarize the conversation so far as a concise handoff so work can \
continue seamlessly without the earlier messages. Do not call any tools. Include:\n\
- The user's goal and key decisions or constraints\n\
- Files inspected, created, or modified (with exact paths)\n\
- Important findings, errors, or command results\n\
- Remaining tasks and next steps";

const COMPACT_HEADER: &str = "Summary of the earlier conversation:";
const COMPACT_ACK: &str = "Understood. Continuing from the compacted summary.";
const COMPACT_CONTINUE: &str = "Continue with the task from the compacted summary above.";

fn compact_instruction(focus: Option<&str>) -> Cow<'static, str> {
    match focus.map(str::trim).filter(|s| !s.is_empty()) {
        Some(focus) => Cow::Owned(format!(
            "{COMPACT_PROMPT}\n\nPay particular attention to: {focus}"
        )),
        None => Cow::Borrowed(COMPACT_PROMPT),
    }
}

/// Replace the outputs of the tool calls before the last model turn with
/// [`PRUNED`], returning how many were replaced. The last turn's outputs
/// stay: the model needs them to go on.
fn prune_tool_outputs(messages: &mut [ChatMessage]) -> usize {
    let Some(last) = messages.iter().rposition(|m| m.role == ChatRole::Assistant) else {
        return 0;
    };
    let mut pruned = 0;
    for part in messages[..last]
        .iter_mut()
        .flat_map(|m| m.content.iter_mut())
    {
        match part {
            ContentPart::ToolResponse(resp) if resp.content.len() > PRUNED.len() => {
                resp.content = PRUNED.to_owned();
            }
            ContentPart::Binary(_) => *part = ContentPart::Text(PRUNED_IMAGE.to_owned()),
            _ => continue,
        }
        pruned += 1;
    }
    pruned
}

/// Move an image a tool read from its `details` to `images`, as the model
/// gets it in a message of its own.
fn take_image(details: Option<ToolDetails>, images: &mut Vec<ContentPart>) -> Option<ToolDetails> {
    match details {
        Some(ToolDetails::Image(Image { mime, data })) => {
            images.push(ContentPart::Binary(Binary::from_base64(mime, data, None)));
            None
        }
        details => details,
    }
}

fn same_call(a: &ToolCall, b: &ToolCall) -> bool {
    a.fn_name == b.fn_name && a.fn_arguments == b.fn_arguments
}

fn same_calls(a: &[&ToolCall], b: &[ToolCall]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_call(x, y))
}

/// Whether `call` plus its earlier occurrences in `prior` reaches
/// [`MAX_REPEATED_CALLS`] within the same turn.
fn is_within_turn_repeat(prior: &[ToolCall], call: &ToolCall) -> bool {
    prior.iter().filter(|c| same_call(c, call)).count() + 1 >= MAX_REPEATED_CALLS
}

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
