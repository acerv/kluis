// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The agent loop.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
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

mod compact;
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

/// Read-only subagents running at the same time.
const MAX_PARALLEL_TASKS: usize = 4;

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
    /// A read-only subagent: it only has the tools that change nothing.
    read_only: bool,
    /// The images the user attached to the next prompt, named.
    attached: Vec<Binary>,
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
            read_only: false,
            attached: Vec::new(),
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
            read_only: false,
            attached: Vec::new(),
        }
    }

    /// Keep only the tools that change nothing, and refuse the others.
    fn into_read_only(mut self) -> Self {
        if let Some(tools) = &mut self.req.tools {
            tools.retain(|tool| allowed_in_plan(tool.name.as_ref()));
        }
        self.read_only = true;
        self
    }

    /// Answer a call of the `task` tool with a new subagent made from
    /// this one, which is a subagent too, starting from an empty
    /// conversation: the answer to its prompt is the result. `on_event`
    /// gets what the UI shows of the work: the tool calls as output of the
    /// call, after `tag`, the token usage and the retries.
    ///
    /// The future is boxed, as it runs in the loop of another agent.
    fn run_task<'a>(
        &'a self,
        call: &'a ToolCall,
        tag: &'a str,
        on_event: &'a mut (dyn FnMut(Event<'_>) + Send),
    ) -> Pin<Box<dyn Future<Output = ToolResponse> + Send + 'a>> {
        Box::pin(async move {
            let task = match TaskTool::deserialize(&call.fn_arguments) {
                Ok(task) if !task.prompt.trim().is_empty() => task,
                Ok(_) => return error_response(call, "prompt must not be empty"),
                Err(err) => return error_response(call, &err.to_string()),
            };
            let mut agent = self.subagent();
            if task.read_only {
                agent = agent.into_read_only();
            }
            let run = agent.prompt(&task.prompt, |event| match event {
                Event::ToolCall(call) => {
                    let line: String = format!("{tag}{} {}", call.fn_name, call.fn_arguments)
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
        self.attached = Vec::new();
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

    /// Attach `image` to the next prompt: the model sees it after the
    /// prompt, under `name`, the words the prompt names it with.
    pub fn attach_image(&mut self, name: impl Into<String>, image: Image) {
        let Image { mime, data } = image;
        self.attached
            .push(Binary::from_base64(mime, data, Some(name.into())));
    }

    /// Remove the last message the user sent, and all after it, from the
    /// history, returning its text to send it again. The images attached
    /// to it are attached to the next prompt again.
    pub fn take_last_prompt(&mut self) -> Option<String> {
        // Its text comes first, before the files it names.
        let idx = self.req.messages.iter().rposition(|m| {
            m.role == ChatRole::User
                && matches!(m.content.parts().first(), Some(ContentPart::Text(text)) if text != IMAGES)
        })?;
        let message = &self.req.messages[idx];
        let text = message.content.first_text()?.to_owned();
        // Only attached images are named: the files it names are read
        // again.
        self.attached = message
            .content
            .parts()
            .iter()
            .filter_map(|part| match part {
                ContentPart::Binary(image) if image.name.is_some() => Some(image.clone()),
                _ => None,
            })
            .collect();
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
    /// `@path` attached as the `read` tool reads them for the model, then
    /// the `attached` images. A name that is no file it can read stays a
    /// word of the text.
    async fn user_message(&self, text: &str, attached: Vec<Binary>) -> ChatMessage {
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
        for image in attached {
            let name = image.name.as_deref().unwrap_or_default();
            parts.push(ContentPart::Text(format!("{name}:")));
            parts.push(ContentPart::Binary(image));
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
    /// `read` tool reads them: text up to its limits, or an image. The
    /// images given to [`Self::attach_image`] follow them.
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
        let attached = std::mem::take(&mut self.attached);
        let message = self.user_message(text, attached).await;
        self.req.messages.push(message);
        self.planned = self.plan;

        let mut turns = 0usize;
        let mut repeats = Repeats::default();
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
                    match compact::prune_tool_outputs(&mut self.req.messages) {
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
            let need_compact =
                compact::is_due(self.auto_compact_tokens, context, &mut compact_blocked);

            if !content.contains_tool_call() {
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

            let repeated = repeats.turn(&self.req.messages, assistant_idx, turn.truncated);
            let refused = Refused {
                truncated: turn.truncated,
                repeated,
                invalid: &turn.invalid,
            };
            self.run_calls(assistant_idx, refused, &mut on_event).await;

            if need_compact && self.max_turns.is_none_or(|limit| turns < limit) {
                if self.run_auto_compact(context, true, &mut on_event).await {
                    saved = self.req.messages.len();
                    repeats = Repeats::default();
                }
                compact_blocked = true;
            }
        }
    }

    /// Run the tool calls of the model turn at `turn` in the history, and
    /// add their results to it, in order. The `refused` ones do not run.
    ///
    /// Dropping the future answers the calls left as cancelled.
    async fn run_calls(
        &mut self,
        turn: usize,
        refused: Refused<'_>,
        on_event: &mut (impl FnMut(Event<'_>) + Send),
    ) {
        // A copy, so results can join the history while calls run.
        let content = &self.req.messages[turn].content;
        let calls: Vec<ToolCall> = content.tool_calls().into_iter().cloned().collect();
        let (plan, read_only) = (self.plan, self.read_only);
        // Why the call at `idx` must not run, if so.
        let refusal = |idx: usize, call: &ToolCall| {
            if refused.truncated {
                Some(TRUNCATED)
            } else if plan && !allowed_in_plan(&call.fn_name) {
                Some(PLAN_REFUSAL)
            } else if read_only && !allowed_in_plan(&call.fn_name) {
                Some(READ_ONLY_REFUSAL)
            } else if refused.invalid.contains(&call.call_id) {
                Some(INVALID_ARGUMENTS)
            } else if refused.repeated || is_within_turn_repeat(&calls[..idx], call) {
                Some(REPEATED_CALL)
            } else {
                None
            }
        };
        // Made before the history is borrowed for the results: the
        // subagents of the tasks are made from it.
        let sub = (!self.sub && calls.iter().any(|c| c.fn_name == tools::TASK_TOOL))
            .then(|| self.subagent());
        let sub = sub.as_ref();
        let (host, executor) = (&self.host, &*self.executor);
        let mut pending = Pending {
            messages: &mut self.req.messages,
            calls: &calls,
            answered: 0,
            images: Vec::new(),
        };
        let mut idx = 0;
        while idx < calls.len() {
            // Consecutive calls that change nothing (`read`, `ls`, `find`,
            // `grep`, and up to `MAX_PARALLEL_TASKS` read-only tasks) run
            // together; their results keep the order of the calls.
            let mut tasks = 0;
            let together = calls[idx..]
                .iter()
                .take_while(|call| {
                    if refused.all() {
                        false
                    } else if sub.is_some() && is_read_only_task(call) {
                        tasks += 1;
                        tasks <= MAX_PARALLEL_TASKS
                    } else {
                        tools::is_read_only(&call.fn_name)
                    }
                })
                .count();
            if together > 1 {
                let batch = &calls[idx..idx + together];
                // What the subagents report meanwhile, each line tagged
                // with the number of its task.
                let events = Mutex::new(&mut *on_event);
                let results = join_all(batch.iter().enumerate().map(|(off, call)| {
                    let refusal = refusal(idx + off, call);
                    let events = &events;
                    async move {
                        let tag = format!("[{}] ", off + 1);
                        let mut report = |event: Event<'_>| {
                            (events.lock().unwrap_or_else(|err| err.into_inner()))(event)
                        };
                        answer(host, executor, call, refusal, sub, &tag, &mut report).await
                    }
                }))
                .await;
                for (call, result) in batch.iter().zip(results) {
                    on_event(Event::ToolCall(call));
                    pending.answer(result, on_event);
                }
                idx += together;
            } else {
                let call = &calls[idx];
                on_event(Event::ToolCall(call));
                let refusal = refusal(idx, call);
                let result = answer(host, executor, call, refusal, sub, "", on_event).await;
                pending.answer(result, on_event);
                idx += 1;
            }
        }
        // Sent after all the results: a message between them would split
        // the turn.
        let mut images = std::mem::take(&mut pending.images);
        drop(pending);
        if !images.is_empty() {
            images.insert(0, ContentPart::Text(IMAGES.to_owned()));
            self.req
                .messages
                .push(ChatMessage::user(MessageContent::from_parts(images)));
        }
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

/// Result of a tool call of a read-only subagent that would change something.
const READ_ONLY_REFUSAL: &str = "read-only task: only tools that change nothing may run";

/// Whether `call` asks for a read-only task.
fn is_read_only_task(call: &ToolCall) -> bool {
    call.fn_name == tools::TASK_TOOL
        && TaskTool::deserialize(&call.fn_arguments).is_ok_and(|task| task.read_only)
}

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

/// A call of the tool `name` made for the user, not the model.
fn user_call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "user".to_owned(),
        fn_name: name.to_owned(),
        fn_arguments: args,
        thought_signatures: None,
    }
}

/// The result of `call`: `refusal` when it must not run, else what a
/// subagent made from `sub` (with `tag` before its lines), a host tool or
/// the executor answers. `on_event` gets what it reports meanwhile.
async fn answer(
    host: &HostTools,
    executor: &Executor,
    call: &ToolCall,
    refusal: Option<&'static str>,
    sub: Option<&Agent>,
    tag: &str,
    on_event: &mut (dyn FnMut(Event<'_>) + Send),
) -> (ToolResponse, Option<ToolDetails>) {
    if let Some(why) = refusal {
        return (error_response(call, why), None);
    }
    if let (tools::TASK_TOOL, Some(sub)) = (call.fn_name.as_str(), sub) {
        return (sub.run_task(call, tag, on_event).await, None);
    }
    if let Some(resp) = host.run(call).await {
        return (resp, None);
    }
    executor
        .execute(call, |chunk| on_event(Event::ToolOutput(chunk)))
        .await
}

fn same_call(a: &ToolCall, b: &ToolCall) -> bool {
    a.fn_name == b.fn_name && a.fn_arguments == b.fn_arguments
}

/// The tool calls of a turn that must not run.
struct Refused<'a> {
    /// All: the turn was cut at the token limit.
    truncated: bool,
    /// All: the turn repeats the ones before it.
    repeated: bool,
    /// The ones with these ids: their arguments could not be read.
    invalid: &'a [String],
}

impl Refused<'_> {
    fn all(&self) -> bool {
        self.truncated || self.repeated
    }
}

/// The turns in a row that made the same tool calls.
#[derive(Default)]
struct Repeats {
    /// Where the last turn is in the history, unless it was truncated.
    prev: Option<usize>,
    streak: usize,
}

impl Repeats {
    /// Count the model turn at `turn` in `messages`, and tell whether it
    /// makes the same calls as the turns before it for the
    /// [`MAX_REPEATED_CALLS`]th time. A `truncated` turn starts the count
    /// again.
    fn turn(&mut self, messages: &[ChatMessage], turn: usize, truncated: bool) -> bool {
        let calls = |idx: usize| messages[idx].content.tool_calls();
        let same = |a: Vec<&ToolCall>, b: Vec<&ToolCall>| {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_call(x, y))
        };
        self.streak = match self.prev {
            _ if truncated => 0,
            Some(prev) if same(calls(prev), calls(turn)) => self.streak + 1,
            _ => 1,
        };
        self.prev = (!truncated).then_some(turn);
        self.streak >= MAX_REPEATED_CALLS
    }
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
    /// The images the tools read: the model gets them in a message of
    /// their own.
    images: Vec<ContentPart>,
}

impl Pending<'_> {
    /// Report the result of the next call and add it to the history.
    fn answer(
        &mut self,
        (resp, details): (ToolResponse, Option<ToolDetails>),
        on_event: &mut impl FnMut(Event<'_>),
    ) {
        let details = match details {
            Some(ToolDetails::Image(Image { mime, data })) => {
                let image = Binary::from_base64(mime, data, None);
                self.images.push(ContentPart::Binary(image));
                None
            }
            details => details,
        };
        on_event(Event::ToolResult(&resp, details.as_ref()));
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
