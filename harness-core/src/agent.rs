// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The agent loop.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use genai::adapter::AdapterKind;
use genai::chat::{
    Binary, ChatMessage, ChatRequest, ChatRole, ContentPart, MessageContent, ToolCall, ToolResponse,
};
use genai::Client;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::mention::mentions;
use crate::models::{discover_models, thinking_levels, ModelList};
use crate::oauth::OAuthLogins;
use crate::retry::{is_too_large, RetryPolicy};
use crate::session::TokenUsage;
use crate::tools::{
    self, error_response, AskTool, Asker, BashTool, Executor, FetchTool, HostTools, Image,
    ReadOutput, SearchTool, Searcher, SkillTool, TaskOutput, TaskTool, ToolContext, ToolDetails,
    WebClient,
};
use crate::{Event, Skill};

mod request;
mod thinking;

pub use thinking::Thinking;

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

/// Characters shown of a tool call a subagent makes.
const MAX_TASK_LINE: usize = 200;

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
    /// Shared with the subagents.
    executor: Arc<Executor>,
    host: HostTools,
    retry: RetryPolicy,
    max_turns: Option<usize>,
    auto_compact_tokens: Option<u64>,
    steering: Steering,
    /// Plan mode: the model may only look, not change anything.
    plan: bool,
    /// Whether the last prompt was sent in plan mode.
    planned: bool,
    /// A subagent: it starts none itself.
    sub: bool,
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
            req: ChatRequest::default()
                .with_tools(tools::definitions())
                .append_tool(TaskTool::definition()),
            cache_key: Uuid::new_v4(),
            executor: Arc::default(),
            host: HostTools::default(),
            retry: RetryPolicy::default(),
            max_turns: Some(DEFAULT_MAX_TURNS),
            auto_compact_tokens: Some(DEFAULT_AUTO_COMPACT_TOKENS),
            steering: Steering::default(),
            plan: false,
            planned: false,
            sub: false,
        }
    }

    /// An agent for a task this one hands off: the same model and tools,
    /// in the same mode, with a conversation of its own. It cannot hand
    /// off tasks, ask the user nor keep a todo list.
    fn subagent(&self) -> Self {
        let mut tools = self.req.tools.clone();
        if let Some(tools) = &mut tools {
            tools.retain(|tool| {
                ![tools::TASK_TOOL, tools::ASK_TOOL, tools::TODO_TOOL].contains(&tool.name.as_ref())
            });
        }
        Self {
            client: self.client.clone(),
            oauth: self.oauth.clone(),
            model: self.model.clone(),
            thinking: self.thinking,
            req: ChatRequest {
                system: self.req.system.clone(),
                tools,
                ..ChatRequest::default()
            },
            cache_key: Uuid::new_v4(),
            executor: self.executor.clone(),
            host: HostTools {
                asker: None,
                ..self.host.clone()
            },
            retry: self.retry,
            max_turns: self.max_turns,
            auto_compact_tokens: self.auto_compact_tokens,
            steering: Steering::default(),
            plan: self.plan,
            planned: false,
            sub: true,
        }
    }

    /// Answer a call of the `task` tool as its subagent, starting from an
    /// empty conversation: the answer to its prompt is the result.
    /// `on_event` gets what the UI shows of the work: the tool calls as
    /// output of the call, the token usage and the retries.
    ///
    /// The future is boxed, as it runs in the loop of another agent.
    fn run_task<'a>(
        &'a mut self,
        call: &'a ToolCall,
        on_event: &'a mut (dyn FnMut(Event<'_>) + Send),
    ) -> Pin<Box<dyn Future<Output = ToolResponse> + Send + 'a>> {
        Box::pin(async move {
            let task = match TaskTool::deserialize(&call.fn_arguments) {
                Ok(task) if !task.prompt.trim().is_empty() => task,
                Ok(_) => return error_response(call, "prompt must not be empty"),
                Err(err) => return error_response(call, &err.to_string()),
            };
            self.clear();
            let run = self.prompt(&task.prompt, |event| match event {
                Event::ToolCall(call) => {
                    let line: String = format!("{} {}", call.fn_name, call.fn_arguments)
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(MAX_TASK_LINE)
                        .chain(['\n'])
                        .collect();
                    on_event(Event::ToolOutput(&line));
                }
                Event::TurnEnd(Some(usage)) => on_event(Event::TaskUsage(usage)),
                Event::Retry { .. } => on_event(event),
                _ => {}
            });
            match run.await {
                Ok(answer) => {
                    let content = json!(TaskOutput { answer }).to_string();
                    ToolResponse::from_tool_call(call, content)
                }
                Err(err) => error_response(call, &err.to_string()),
            }
        })
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

    /// Switch plan mode for subsequent prompts: the model gets only the
    /// tools that change nothing and is asked for a plan, until the user
    /// turns it off to have the plan carried out.
    pub fn set_plan(&mut self, plan: bool) {
        self.plan = plan;
    }

    /// Whether plan mode is on.
    pub fn plan(&self) -> bool {
        self.plan
    }

    /// Set the tool executor used to run model tool calls.
    pub fn with_executor(mut self, executor: Executor) -> Self {
        self.executor = Arc::new(executor);
        self
    }

    /// Offer the `background` option of `bash`, which the executor must
    /// allow too (see `BashContext::background`).
    pub fn with_background_commands(mut self) -> Self {
        for tool in self.req.tools.iter_mut().flatten() {
            if tool.name.as_ref() == tools::BASH_TOOL {
                *tool = BashTool::definition(true);
            }
        }
        self
    }

    /// Offer the `ask_user` tool, sending its questions through `asker`.
    pub fn with_asker(mut self, asker: Asker) -> Self {
        if self.host.asker.replace(asker).is_none() {
            self.req = self.req.append_tool(AskTool::definition());
        }
        self
    }

    /// Offer the `web_fetch` tool, which reaches the network from the host
    /// through `client`.
    pub fn with_web_fetch(mut self, client: WebClient) -> Self {
        if self.host.fetcher.replace(client).is_none() {
            self.req = self.req.append_tool(FetchTool::definition());
        }
        self
    }

    /// Offer the `web_search` tool, which reaches the network from the host
    /// through `searcher`.
    pub fn with_web_search(mut self, searcher: Searcher) -> Self {
        if self.host.searcher.replace(searcher).is_none() {
            self.req = self.req.append_tool(SearchTool::definition());
        }
        self
    }

    /// Offer the `skill` tool, loading one of `skills`.
    pub fn with_skills(mut self, skills: Vec<Skill>) -> Self {
        if self.host.skills.is_empty() && !skills.is_empty() {
            self.req = self.req.append_tool(SkillTool::definition(&skills));
            self.host.skills = skills;
        }
        self
    }

    /// The skills the `skill` tool loads.
    pub fn skills(&self) -> &[Skill] {
        &self.host.skills
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
    pub fn with_tool_context(self, tools: ToolContext) -> Self {
        self.with_executor(Executor::Local(tools))
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

    /// Thinking levels the current model takes, as its provider reports
    /// them; `None` when the provider does not say.
    pub async fn thinking_levels(&self) -> Option<Vec<Thinking>> {
        thinking_levels(&self.client, &self.model).await
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

    /// Remove the last message the user sent, and all after it, from the
    /// history, returning its text to send it again.
    pub fn take_last_prompt(&mut self) -> Option<String> {
        // Its text comes first, before the files it names.
        let idx = self.req.messages.iter().rposition(|m| {
            m.role == ChatRole::User
                && matches!(m.content.parts().first(), Some(ContentPart::Text(text)) if text != IMAGES)
        })?;
        let text = self.req.messages[idx].content.first_text()?.to_owned();
        self.req.messages.truncate(idx);
        Some(text)
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
        let call = user_call(tools::BASH_TOOL, json!({ "command": command }));
        self.executor.execute(&call, on_output).await.0.content
    }

    /// The message of the user saying `text`, with the files it names with
    /// `@path` attached as the `read` tool reads them for the model. A name
    /// that is no file it can read stays a word of the text.
    async fn user_message(&self, text: &str) -> ChatMessage {
        let mut parts = vec![ContentPart::Text(text.to_owned())];
        for path in mentions(text) {
            let call = user_call(tools::READ_TOOL, json!({ "path": path }));
            let (resp, details) = self.executor.execute(&call, |_| {}).await;
            let Ok(out) = serde_json::from_str::<ReadOutput>(&resp.content) else {
                continue;
            };
            if let Some(ToolDetails::Image(Image { mime, data })) = details {
                parts.push(ContentPart::Text(format!("Image {path}:")));
                parts.push(ContentPart::Binary(Binary::from_base64(mime, data, None)));
                continue;
            }
            let mut file = format!("<file path=\"{path}\">\n{}", out.content);
            if !file.ends_with('\n') {
                file.push('\n');
            }
            if let Some(next) = out.next_offset {
                file.push_str(&format!("[cut: read the rest from line {next}]\n"));
            }
            file.push_str("</file>");
            parts.push(ContentPart::Text(file));
        }
        if self.plan {
            parts.push(ContentPart::Text(PLAN_ON.to_owned()));
        } else if self.planned {
            parts.push(ContentPart::Text(PLAN_OFF.to_owned()));
        }
        ChatMessage::user(MessageContent::from_parts(parts))
    }

    /// Stop what tool commands left running in the background, when they
    /// run in the sandbox: call it between prompts. See
    /// [`Executor::stop_background`].
    pub async fn stop_background(&self, keep_jobs: bool) -> bool {
        self.executor.stop_background(keep_jobs).await
    }

    /// Send a user message, run the tool calls the model makes, and return
    /// the model's final answer.
    ///
    /// Files the message names with `@path` are attached to it, read as the
    /// `read` tool reads them: text up to its limits, or an image.
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
        mut on_event: impl FnMut(Event<'_>) + Send,
    ) -> Result<String, Error> {
        // Where the messages not yet reported with `Event::Messages` start.
        let mut saved = self.req.messages.len();
        let message = self.user_message(text).await;
        self.req.messages.push(message);
        self.planned = self.plan;
        let plan = self.plan;

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
                } else if plan && !allowed_in_plan(&call.fn_name) {
                    Some(PLAN_REFUSAL)
                } else if turn.invalid.contains(&call.call_id) {
                    Some(INVALID_ARGUMENTS)
                } else if repeated_turn || is_within_turn_repeat(&calls[..idx], call) {
                    Some(REPEATED_CALL)
                } else {
                    None
                }
            };
            // Made before the history is borrowed for the results.
            let mut sub = (!self.sub && calls.iter().any(|c| c.fn_name == tools::TASK_TOOL))
                .then(|| self.subagent());
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
                    } else if let (tools::TASK_TOOL, Some(sub)) = (call.fn_name.as_str(), &mut sub)
                    {
                        (sub.run_task(call, &mut on_event).await, None)
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
}

/// Result of a tool call the agent stopped waiting for.
const CANCELLED: &str = "cancelled; the call may have partly run";

/// Result of a tool call from a response truncated by the token limit.
const TRUNCATED: &str = "response was truncated at max_tokens; tool call was not executed — please re-issue it (split into smaller calls if needed)";

/// Told to the model with each prompt in plan mode.
const PLAN_ON: &str = "[Plan mode: look around with the tools that change nothing, \
then answer with a plan for the request. Do not change anything: the user \
approves the plan first.]";

/// Told to the model with the first prompt after plan mode.
const PLAN_OFF: &str = "[Plan mode is off: you may now change files and run commands.]";

/// Result of a tool call that would change something in plan mode.
const PLAN_REFUSAL: &str =
    "plan mode: only tools that change nothing may run until the user approves the plan";

/// Whether the tool `name` may run in plan mode: it changes nothing.
fn allowed_in_plan(name: &str) -> bool {
    tools::is_read_only(name)
        || [
            tools::TODO_TOOL,
            tools::FETCH_TOOL,
            tools::SEARCH_TOOL,
            tools::SKILL_TOOL,
            tools::TASK_TOOL,
        ]
        .contains(&name)
}

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

/// A call of the tool `name` made for the user, not the model.
fn user_call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "user".to_owned(),
        fn_name: name.to_owned(),
        fn_arguments: args,
        thought_signatures: None,
    }
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
