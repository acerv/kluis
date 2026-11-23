// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use genai::chat::{ChatMessage, ChatRole, ContentPart};
use harness_core::tools::{Diff, DiffLine, Todo, TodoStatus, TodoTool, TODO_TOOL};
use harness_core::{
    Event as AgentEvent, ModelInfo, SessionSummary, Thinking, TokenUsage, ToolDetails,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde::Deserialize;
use serde_json::Value;
use tui_markdown::CodeTheme;
use unicode_width::UnicodeWidthStr;

use crate::complete::CommandCompletion;
use crate::input::Input;
use crate::markdown::{render_markdown, MarkdownStream, MarkdownStyle};
use crate::picker::{Picker, PickerKind, PickerOutcome};
use crate::style::Theme;
use crate::terminal::{char_width, wrap_lines, wrapped_line_count};
use crate::Status;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Maximum characters shown in a single-line summary.
const SUMMARY_LEN: usize = 120;

/// Maximum bytes retained in hidden reasoning when thinking is toggled off.
const MAX_HIDDEN_REASONING_BYTES: usize = 64 * 1024;

/// Maximum diff lines shown inline in scrollback per `edit` call.
const MAX_DIFF_PREVIEW_LINES: usize = 24;

/// Maximum output lines shown inline in scrollback per `bash` call.
const MAX_BASH_PREVIEW_LINES: usize = 8;

/// Maximum bytes retained per live tool output preview line.
const MAX_TOOL_PREVIEW_LINE_BYTES: usize = 1024;

/// Maximum visible rows in the live streaming preview above the spinner.
const MAX_LIVE_ROWS: u16 = 6;

/// Maximum `todo` items shown above the editor.
const MAX_TODO_ROWS: usize = 8;

/// Maximum visible rows inside the framed multiline editor before scrolling.
const MAX_EDITOR_ROWS: u16 = 8;

/// Default width used for soft-wrapping before the first terminal size is seen.
const DEFAULT_WIDTH: u16 = 80;

/// Fixed non-editor, non-live chrome rows in the inline viewport:
/// 1 status/spinner line + top rule + bottom rule + 1 footer line.
const FIXED_CHROME_ROWS: u16 = 4;

/// Initial height of the inline viewport (1 live row + 1-row editor + chrome).
pub const VIEWPORT_HEIGHT: u16 = FIXED_CHROME_ROWS + 2;

/// Rows of the startup logo.
const LOGO: [&str; 3] = ["█ ▄▀ █", "█▀▄  █", "▀  ▀ ▀▀▀"];

/// Columns between the logo and the banner text.
const LOGO_GAP: usize = 3;

/// One line of text shown next to the startup logo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerLine {
    pub text: String,
    /// Shown as an error, e.g. missing credentials.
    pub warn: bool,
}

impl BannerLine {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            warn: false,
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            warn: true,
        }
    }
}

/// Owned agent progress update passed over the channel from [`Agent::prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentUpdate {
    TurnStart,
    ReasoningDelta(String),
    TextDelta(String),
    Text(String),
    TurnEnd(TokenUsage),
    /// The turn failed for a passing reason and will be asked again.
    Retry {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    ToolOutput(String),
    ToolResult {
        content: String,
        details: Option<ToolDetails>,
    },
    Compacting(u64),
    Compacted(usize),
    CompactFailed(String),
    /// The request was too large: the outputs of this many old tool calls
    /// were removed.
    Pruned(usize),
    /// The final text of the model was cut at its token limit.
    Truncated,
    /// A message the user queued joined the conversation.
    Steered(String),
}

impl AgentUpdate {
    /// Convert a borrowed [`harness_core::Event`] into an owned update.
    pub fn from_event(event: AgentEvent<'_>) -> Option<Self> {
        Some(match event {
            AgentEvent::Messages(_) => return None,
            AgentEvent::TurnStart => Self::TurnStart,
            AgentEvent::Truncated => Self::Truncated,
            AgentEvent::ReasoningDelta(text) => Self::ReasoningDelta(text.to_owned()),
            AgentEvent::TextDelta(text) => Self::TextDelta(text.to_owned()),
            AgentEvent::Text(text) => Self::Text(text.to_owned()),
            AgentEvent::TurnEnd(usage) => Self::TurnEnd(TokenUsage::from_genai(usage?)),
            AgentEvent::Retry {
                attempt,
                delay,
                reason,
            } => Self::Retry {
                attempt,
                delay,
                reason: reason.to_owned(),
            },
            AgentEvent::ToolCall(call) => Self::ToolCall {
                name: call.fn_name.clone(),
                args: call.fn_arguments.to_string(),
            },
            AgentEvent::ToolOutput(chunk) => Self::ToolOutput(chunk.to_owned()),
            AgentEvent::ToolResult(resp, details) => Self::ToolResult {
                content: resp.content.clone(),
                details: details.cloned(),
            },
            AgentEvent::Compacting { context_tokens } => Self::Compacting(context_tokens),
            AgentEvent::Compacted { before, .. } => Self::Compacted(before),
            AgentEvent::CompactFailed(err) => Self::CompactFailed(err.to_owned()),
            AgentEvent::Pruned(count) => Self::Pruned(count),
            AgentEvent::Steered(text) => Self::Steered(text.to_owned()),
        })
    }
}

/// Interactive slash commands handled locally by the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Model(Option<String>),
    Thinking(Option<String>),
    Resume(Option<String>),
    Compact(Option<String>),
    Retry,
    Status,
    Output,
    Copy,
    New,
    Quit,
    Help,
}

/// High-level action produced by handling a terminal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Submit(String),
    /// A shell command typed after `!`, to run in the sandbox.
    Shell(String),
    Slash(SlashCommand),
    ResumeSession(String),
    SelectModel(String),
    SetThinking(Thinking),
    /// The user's answer to the question of the model.
    Answer(String),
    Cancel,
    Quit,
}

/// Pure TUI state machine, decoupled from terminal I/O so it can be tested
/// directly with ratatui's `TestBackend`.
pub struct App {
    model: String,
    thinking: Thinking,
    status: Status,
    theme: Theme,
    width: Cell<u16>,
    /// Where the last [`App::render`] placed the visible cursor.
    cursor: Cell<Option<Position>>,
    input: Input,
    busy: bool,
    /// Messages typed while busy, waiting to join the conversation.
    queued: usize,
    show_thinking: bool,
    is_thinking: bool,
    spinner: usize,
    activity_started: Option<Instant>,
    active_tool: Option<ToolCall>,
    /// The last finished tool call, shown whole by `/output`.
    last_tool: Option<ToolRun>,
    /// Bounded live stdout/stderr preview while a streaming tool runs.
    tool_preview: ToolPreview,
    /// Incremental Markdown stream for the current assistant turn.
    markdown: MarkdownStream,
    markdown_style: MarkdownStyle,
    /// Current incomplete streaming reasoning row (fits within `width`).
    live_reasoning: String,
    /// Reasoning buffered while `show_thinking` is off so `Ctrl-O` can reveal
    /// the current turn's thinking in real time.
    hidden_reasoning: String,
    /// Whether any reasoning lines were emitted during the current turn.
    emitted_reasoning: bool,
    /// Cumulative token usage across the active conversation.
    usage: TokenUsage,
    /// Tokens the last request sent, as the provider counted them: the
    /// size of the conversation, or 0 before its first request.
    context: u64,
    notice: Option<String>,
    picker: Option<Picker>,
    completion: CommandCompletion,
    /// The plan of the `todo` tool, shown above the editor.
    todos: Vec<Todo>,
    scrollback: Vec<Line<'static>>,
}

impl App {
    pub fn new(model: impl Into<String>, status: Status, theme: Theme) -> Self {
        Self {
            model: model.into(),
            thinking: Thinking::Default,
            status,
            theme,
            width: Cell::new(DEFAULT_WIDTH),
            cursor: Cell::new(None),
            input: Input::default(),
            busy: false,
            queued: 0,
            show_thinking: true,
            is_thinking: false,
            spinner: 0,
            activity_started: None,
            active_tool: None,
            last_tool: None,
            tool_preview: ToolPreview::default(),
            markdown: MarkdownStream::default(),
            markdown_style: MarkdownStyle::new(theme.color, DEFAULT_WIDTH),
            live_reasoning: String::new(),
            hidden_reasoning: String::new(),
            emitted_reasoning: false,
            usage: TokenUsage::default(),
            context: 0,
            notice: None,
            picker: None,
            completion: CommandCompletion::default(),
            todos: Vec::new(),
            scrollback: Vec::new(),
        }
    }

    /// Open the inline session picker with `entries`.
    pub fn open_picker(&mut self, entries: Vec<SessionSummary>, current_id: Option<String>) {
        self.notice = None;
        self.completion.clear();
        self.picker = Some(Picker::sessions(entries, current_id));
    }

    /// Open the inline model picker with `models`.
    pub fn open_model_picker(&mut self, models: &[ModelInfo]) {
        self.notice = None;
        self.completion.clear();
        self.picker = Some(Picker::models(models, &self.model));
    }

    /// Show a question of the model and let the user pick or type the
    /// answer.
    pub fn ask(&mut self, question: &str, options: &[String]) {
        self.notice = None;
        self.completion.clear();
        // The tool call line shows the question cut to one line.
        if summary(question) != question {
            for line in question.lines() {
                self.scrollback.push(Line::raw(format!("  {line}")));
            }
        }
        self.picker = Some(Picker::answers(options));
    }

    pub fn is_picker_open(&self) -> bool {
        self.picker.is_some()
    }

    pub fn is_completion_open(&self) -> bool {
        self.completion.is_open()
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    pub fn thinking(&self) -> Thinking {
        self.thinking
    }

    pub fn set_thinking(&mut self, thinking: Thinking) {
        self.thinking = thinking;
    }

    /// Seed the editor's prompt history (oldest first).
    pub fn set_history(&mut self, entries: Vec<String>) {
        self.input.set_history(entries);
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Highlight code blocks with `theme`.
    pub fn set_code_theme(&mut self, theme: CodeTheme) {
        self.markdown_style.set_code_theme(theme);
    }

    /// Update the terminal width used for wrapping and Markdown table layout.
    pub fn set_width(&mut self, width: u16) {
        if width > 0 {
            self.width.set(width);
            if self.sync_markdown_width() {
                self.markdown.rerender_live(&self.markdown_style);
            }
        }
    }

    fn sync_markdown_width(&mut self) -> bool {
        !self.markdown.has_committed() && self.markdown_style.set_table_width(self.width.get())
    }

    /// Push the logo into scrollback, with `info` next to it.
    pub fn push_banner(&mut self, info: &[BannerLine]) {
        let logo_w = LOGO.iter().map(|row| row.width()).max().unwrap_or(0);
        self.scrollback.push(Line::raw(String::new()));
        for row in 0..LOGO.len().max(info.len()) {
            let logo = LOGO.get(row).copied().unwrap_or_default();
            let logo_style = self.theme.logo.get(row).copied().unwrap_or_default();
            let Some(line) = info.get(row) else {
                self.scrollback
                    .push(Line::styled(logo.to_owned(), logo_style));
                continue;
            };
            let pad = " ".repeat(logo_w - logo.width() + LOGO_GAP);
            let style = if line.warn {
                self.theme.error
            } else {
                self.theme.muted
            };
            self.scrollback.push(Line::from(vec![
                Span::styled(format!("{logo}{pad}"), logo_style),
                Span::styled(line.text.clone(), style),
            ]));
        }
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Push informational muted text into scrollback (one line per `\n`).
    pub fn push_info(&mut self, msg: impl AsRef<str>) {
        let text = msg.as_ref();
        if text.is_empty() {
            self.scrollback.push(Line::raw(String::new()));
            return;
        }
        for line in text.lines() {
            self.scrollback
                .push(Line::styled(line.to_owned(), self.theme.muted));
        }
    }

    /// Forget the conversation and drop pending scrollback (e.g. on `/new`).
    pub fn reset_session(&mut self) {
        self.usage = TokenUsage::default();
        self.context = 0;
        self.clear_live();
        self.scrollback.clear();
        self.todos.clear();
        self.last_tool = None;
        self.notice = None;
    }

    /// Cumulative token usage recorded for the current conversation.
    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    /// Restore cumulative token usage (e.g. from a resumed session).
    pub fn set_usage(&mut self, usage: TokenUsage) {
        self.usage = usage;
    }

    /// Tokens the last request of the conversation sent, 0 when unknown.
    pub fn context(&self) -> u64 {
        self.context
    }

    /// Show `tokens` as the size of the conversation, 0 when unknown, as for
    /// a restored one.
    pub fn set_context(&mut self, tokens: u64) {
        self.context = tokens;
    }

    /// Multi-line summary of the active model, session, context, and token
    /// usage shown by `/status`.
    pub fn status_text(&self, session: Option<&str>, messages: usize) -> String {
        use std::fmt::Write;

        let mut out = String::with_capacity(128);
        let _ = write!(out, "model:   {} ({})", self.model, self.thinking);
        let session_label = session.unwrap_or("off");
        let unit = if messages == 1 { "message" } else { "messages" };
        let _ = write!(out, "\nsession: {session_label} · {messages} {unit}");
        if self.context > 0 {
            let _ = write!(out, "\ncontext: {} tokens", ScaledTokens(self.context));
        }
        if self.usage.is_empty() {
            out.push_str("\ntokens:  0");
        } else {
            let _ = write!(
                out,
                "\ntokens:  {} input · {} cached",
                ScaledTokens(self.usage.input),
                ScaledTokens(self.usage.cached),
            );
            let prompt = self.usage.prompt();
            if prompt > 0 {
                let pct = ((self.usage.cached as u128 * 100 + prompt as u128 / 2) / prompt as u128)
                    as u64;
                let _ = write!(out, " ({pct}% hit)");
            }
            let _ = write!(out, " · {} output", ScaledTokens(self.usage.output));
        }
        out
    }

    /// Replay a restored conversation history into scrollback, with its size
    /// unknown until [`App::set_context`] or its next request.
    pub fn replay_history(&mut self, messages: &[ChatMessage]) {
        self.usage = TokenUsage::default();
        self.context = 0;
        // The plan of the conversation shown until now is not of this one.
        self.todos.clear();
        self.sync_markdown_width();
        let mut tool_calls: HashMap<&str, (&str, &Value)> = HashMap::new();
        let mut last_tool = None;
        for msg in messages {
            match msg.role {
                ChatRole::User => {
                    if let Some(text) = msg.content.joined_texts() {
                        self.push_user_message(&text);
                    }
                }
                ChatRole::Assistant => {
                    for part in msg.content.parts() {
                        match part {
                            ContentPart::Text(text) if !text.is_empty() => {
                                self.scrollback
                                    .extend(render_markdown(text, &self.markdown_style));
                                self.scrollback.push(Line::raw(String::new()));
                            }
                            ContentPart::ToolCall(call) => {
                                tool_calls.insert(
                                    call.call_id.as_str(),
                                    (call.fn_name.as_str(), &call.fn_arguments),
                                );
                                if call.fn_name == TODO_TOOL {
                                    continue;
                                }
                                self.scrollback.push(format_tool_call(
                                    &call.fn_name,
                                    &call.fn_arguments.to_string(),
                                    self.theme,
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                ChatRole::Tool => {
                    for resp in msg.content.tool_responses() {
                        let entry = tool_calls.remove(resp.call_id.as_str());
                        if let Some((TODO_TOOL, args)) = entry {
                            self.set_todos(&resp.content, None, Some(args));
                            continue;
                        }
                        self.push_tool_result(entry.map(|(name, _)| name), &resp.content, None);
                        last_tool = Some((entry, resp.content.as_str()));
                    }
                }
                ChatRole::System => {}
            }
        }
        self.last_tool = last_tool.map(|(entry, content)| ToolRun {
            call: entry.map(|(name, args)| ToolCall {
                name: name.to_owned(),
                args: args.to_string(),
            }),
            content: content.to_owned(),
            details: None,
        });
        self.drop_finished_plan();
    }

    /// Show the preview of a finished tool call.
    fn push_tool_result(
        &mut self,
        tool: Option<&str>,
        content: &str,
        details: Option<&ToolDetails>,
    ) {
        format_tool_result(tool, content, details, self.theme, &mut self.scrollback);
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Show the whole output of the last tool call, as `/output` asks.
    pub fn show_tool_output(&mut self) {
        let Some(run) = &self.last_tool else {
            self.push_info("[no tool output]");
            return;
        };
        format_full_output(run, self.theme, &mut self.scrollback);
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Show the plan of a `todo` result above the editor, or its error in
    /// scrollback.
    fn set_todos(
        &mut self,
        content: &str,
        details: Option<ToolDetails>,
        call_args: Option<&Value>,
    ) {
        if let Some(ToolDetails::Todo(todos)) = details {
            self.todos = todos;
            return;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(content) {
            if parsed.get("error").is_none() {
                if let Some(plan) = call_args
                    .and_then(|args| TodoTool::deserialize(args).ok())
                    .or_else(|| TodoTool::deserialize(&parsed).ok())
                {
                    self.todos = plan.todos;
                    return;
                }
            }
        }
        self.scrollback
            .push(Line::styled(TODO_TOOL, self.theme.tool_name));
        format_tool_result(None, content, None, self.theme, &mut self.scrollback);
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Hide the plan once all of it is done.
    fn drop_finished_plan(&mut self) {
        if self.todos.iter().all(|t| t.status == TodoStatus::Completed) {
            self.todos.clear();
        }
    }

    /// Take lines queued for native terminal scrollback.
    pub fn take_scrollback(&mut self) -> Vec<Line<'static>> {
        std::mem::take(&mut self.scrollback)
    }

    /// Advance the activity spinner while busy.
    pub fn tick(&mut self) {
        if self.busy {
            self.spinner = (self.spinner + 1) % SPINNER.len();
        }
    }

    /// Override the elapsed activity timer (useful for deterministic rendering tests).
    pub fn set_elapsed(&mut self, elapsed: Duration) {
        self.activity_started = Instant::now().checked_sub(elapsed);
    }

    /// Record a submitted user prompt and enter the busy state.
    pub fn start_prompt(&mut self, text: &str) {
        self.enter_busy();
        self.push_user_message(text);
    }

    /// Enter the busy state for a background model operation such as `/compact`.
    pub fn start_activity(&mut self) {
        self.enter_busy();
    }

    fn enter_busy(&mut self) {
        self.busy = true;
        self.spinner = 0;
        self.activity_started = Some(Instant::now());
        self.active_tool = None;
        self.clear_live();
        self.completion.clear();
        self.notice = None;
    }

    fn push_user_message(&mut self, text: &str) {
        self.scrollback.push(Line::raw(String::new()));
        for line in text.lines() {
            if self.theme.color {
                self.scrollback
                    .push(Line::styled(format!(" {line} "), self.theme.user));
            } else {
                self.scrollback.push(Line::raw(format!("> {line}")));
            }
        }
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Apply a streaming update from the running agent turn.
    pub fn apply_update(&mut self, update: AgentUpdate) {
        match update {
            AgentUpdate::TurnStart => {
                self.active_tool = None;
                self.activity_started = Some(Instant::now());
                self.clear_live();
            }
            AgentUpdate::ReasoningDelta(delta) => {
                self.is_thinking = true;
                if !self.show_thinking {
                    append_bounded(
                        &mut self.hidden_reasoning,
                        &delta,
                        MAX_HIDDEN_REASONING_BYTES,
                    );
                    return;
                }
                self.emitted_reasoning = true;
                stream_reasoning(
                    &mut self.live_reasoning,
                    &delta,
                    self.width.get() as usize,
                    self.theme.reasoning,
                    &mut self.scrollback,
                );
            }
            AgentUpdate::TextDelta(delta) => {
                self.flush_reasoning();
                self.sync_markdown_width();
                self.markdown
                    .push_delta(&delta, &self.markdown_style, &mut self.scrollback);
            }
            AgentUpdate::Text(text) => {
                self.flush_reasoning();
                self.finish_markdown(Some(&text));
            }
            AgentUpdate::TurnEnd(usage) => {
                self.usage += usage;
                self.context = usage.prompt();
            }
            AgentUpdate::Retry {
                attempt,
                delay,
                reason,
            } => {
                // Keep what the failed attempt showed, marked as such.
                self.flush_reasoning();
                self.flush_text();
                self.scrollback.push(Line::styled(
                    retry_notice(attempt, delay, &reason),
                    self.theme.muted,
                ));
            }
            AgentUpdate::ToolCall { name, args } => {
                self.flush_reasoning();
                self.flush_text();
                self.tool_preview.clear();
                self.activity_started = Some(Instant::now());
                if name != TODO_TOOL {
                    self.scrollback
                        .push(format_tool_call(&name, &args, self.theme));
                }
                self.active_tool = Some(ToolCall { name, args });
            }
            AgentUpdate::ToolOutput(chunk) => {
                self.tool_preview.push(&chunk);
            }
            AgentUpdate::ToolResult { content, details } => {
                let call = self.active_tool.take();
                self.tool_preview.clear();
                self.activity_started = Some(Instant::now());
                if call.as_ref().is_some_and(|c| c.name == TODO_TOOL) {
                    self.set_todos(&content, details, None);
                    return;
                }
                let tool = call.as_ref().map(|c| c.name.as_str());
                self.push_tool_result(tool, &content, details.as_ref());
                self.last_tool = Some(ToolRun {
                    call,
                    content,
                    details,
                });
            }
            AgentUpdate::Compacting(tokens) => {
                self.push_notice(
                    format!("[auto-compacting ({} tokens)…]", ScaledTokens(tokens)),
                    self.theme.muted,
                );
            }
            AgentUpdate::Compacted(before) => {
                self.context = 0;
                self.push_notice(format!("[compacted {before} messages]"), self.theme.muted);
            }
            AgentUpdate::CompactFailed(err) => {
                self.push_notice(format!("[auto-compact failed: {err}]"), self.theme.error);
            }
            AgentUpdate::Pruned(count) => {
                self.push_notice(pruned_notice(count), self.theme.muted);
            }
            AgentUpdate::Truncated => {
                self.push_notice(TRUNCATED_NOTICE.to_owned(), self.theme.error);
            }
            AgentUpdate::Steered(text) => {
                self.flush_reasoning();
                self.flush_text();
                self.queued = self.queued.saturating_sub(1);
                self.push_user_message(&text);
            }
        }
    }

    fn push_notice(&mut self, text: String, style: Style) {
        self.flush_reasoning();
        self.flush_text();
        self.scrollback.push(Line::styled(text, style));
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Leave the busy state when the prompt finishes or fails.
    pub fn finish_prompt(&mut self, outcome: Result<(), String>) {
        self.flush_reasoning();
        self.flush_text();
        self.busy = false;
        self.queued = 0;
        self.activity_started = None;
        self.active_tool = None;
        self.clear_live();
        self.drop_finished_plan();
        if let Err(err) = outcome {
            self.scrollback
                .push(Line::styled(format!("error: {err}"), self.theme.error));
            self.scrollback.push(Line::raw(String::new()));
        }
    }

    /// Put messages queued for a cancelled prompt back in the editor, before
    /// what is typed there.
    pub fn restore_queued(&mut self, text: &str) {
        let typed = self.input.text();
        let text = if typed.is_empty() {
            text.to_owned()
        } else {
            format!("{text}\n\n{typed}")
        };
        self.input.set_text(&text);
        self.completion.update(self.input.text());
    }

    /// Leave the busy state after the user cancels an in-flight prompt.
    pub fn cancel_prompt(&mut self) {
        self.flush_reasoning();
        self.flush_text();
        self.busy = false;
        self.queued = 0;
        self.activity_started = None;
        self.active_tool = None;
        self.clear_live();
        // The work was dropped; the model sends the plan again to go on.
        self.todos.clear();
        self.scrollback
            .push(Line::styled("[cancelled]", self.theme.muted));
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Process a crossterm terminal event and return the next action.
    pub fn handle_event(&mut self, event: TermEvent) -> Action {
        match event {
            TermEvent::Resize(w, _) => {
                if w > 0 {
                    self.set_width(w);
                    if self.show_thinking && !self.live_reasoning.is_empty() {
                        stream_reasoning(
                            &mut self.live_reasoning,
                            "",
                            w as usize,
                            self.theme.reasoning,
                            &mut self.scrollback,
                        );
                    }
                }
                Action::Redraw
            }
            TermEvent::Paste(text) => {
                if let Some(picker) = &mut self.picker {
                    return if picker.paste(&text) {
                        Action::Redraw
                    } else {
                        Action::None
                    };
                }
                self.notice = None;
                if self.input.paste(&text) {
                    self.completion.update(self.input.text());
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            _ => Action::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if let Some(picker) = &mut self.picker {
            return match picker.handle_key(key) {
                PickerOutcome::None => Action::None,
                PickerOutcome::Redraw => Action::Redraw,
                PickerOutcome::Select(kind, id) => {
                    self.picker = None;
                    match kind {
                        PickerKind::Session => Action::ResumeSession(id),
                        PickerKind::Model => Action::SelectModel(id),
                        PickerKind::Answer => Action::Answer(id),
                    }
                }
                PickerOutcome::Cancel => match self.picker.take().map(|p| p.kind()) {
                    // The model waits for the answer: cancel the prompt.
                    Some(PickerKind::Answer) => Action::Cancel,
                    _ => Action::Redraw,
                },
                PickerOutcome::Quit => {
                    self.picker = None;
                    Action::Quit
                }
            };
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('o'), true) => {
                self.toggle_thinking();
                return Action::Redraw;
            }
            (KeyCode::Char('c'), true) => {
                if self.busy {
                    return Action::Cancel;
                }
                if !self.input.is_empty() {
                    self.input.clear();
                    self.completion.clear();
                    self.notice = None;
                } else {
                    self.notice = Some("Press Ctrl-D to quit".to_owned());
                }
                return Action::Redraw;
            }
            (KeyCode::Esc, false) if self.busy => return Action::Cancel,
            (KeyCode::Char('d'), true) => {
                if self.busy || self.input.is_empty() {
                    return Action::Quit;
                }
                return Action::None;
            }
            _ => {}
        }

        if key.code == KeyCode::BackTab && !self.busy {
            let next = self.thinking.next();
            self.thinking = next;
            self.notice = Some(format!("Thinking: {next} (Shift+Tab to cycle)"));
            return Action::SetThinking(next);
        }

        if self.completion.is_open() {
            match (key.code, ctrl) {
                (KeyCode::Esc, false) => {
                    self.completion.dismiss();
                    return Action::Redraw;
                }
                (KeyCode::Up, false) | (KeyCode::Char('p'), true) => {
                    return if self.completion.move_up() {
                        Action::Redraw
                    } else {
                        Action::None
                    };
                }
                (KeyCode::Down, false) | (KeyCode::Char('n'), true) => {
                    return if self.completion.move_down() {
                        Action::Redraw
                    } else {
                        Action::None
                    };
                }
                (KeyCode::Tab, false) => {
                    if let Some(insert) = self.completion.selected_item().map(|i| i.insert.clone())
                    {
                        self.input.set_text(&insert);
                        self.completion.update(self.input.text());
                        return Action::Redraw;
                    }
                }
                _ => {}
            }
        }

        self.notice = None;
        if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            if let Some(insert) = self.completion.selected_item().map(|i| i.insert.clone()) {
                let trimmed = self.input.text().trim();
                let already_complete = trimmed.eq_ignore_ascii_case(insert.trim())
                    || matches!(parse_slash(trimmed), Some(Ok(SlashCommand::Thinking(None))));
                if !already_complete || self.completion.navigated() {
                    if insert.ends_with(' ') {
                        self.input.set_text(&insert);
                        self.completion.update(self.input.text());
                        return Action::Redraw;
                    }
                    self.input.set_text(&insert);
                }
            }
            // Commands wait for the prompt to end.
            if self.busy && self.input.text().trim_start().starts_with(['/', '!']) {
                return Action::None;
            }
            self.completion.clear();
            let text = self.input.submit();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Action::Redraw;
            }
            if let Some(action) = parse_slash(trimmed) {
                return match action {
                    Ok(cmd) => Action::Slash(cmd),
                    Err(unknown) => {
                        self.push_info(format!("unknown command: {unknown} (try /help)"));
                        Action::Redraw
                    }
                };
            }
            if let Some(command) = trimmed.strip_prefix('!') {
                return Action::Shell(command.trim_start().to_owned());
            }
            if self.busy {
                self.queued += 1;
            }
            return Action::Submit(trimmed.to_owned());
        }

        if self.input.handle_key(key) {
            self.completion.update(self.input.text());
            Action::Redraw
        } else {
            Action::None
        }
    }

    fn toggle_thinking(&mut self) {
        self.show_thinking = !self.show_thinking;
        if self.show_thinking {
            if !self.hidden_reasoning.is_empty() {
                let hidden = std::mem::take(&mut self.hidden_reasoning);
                self.emitted_reasoning = true;
                stream_reasoning(
                    &mut self.live_reasoning,
                    &hidden,
                    self.width.get() as usize,
                    self.theme.reasoning,
                    &mut self.scrollback,
                );
            }
            self.notice = Some("Thinking visible (Ctrl-O to hide)".to_owned());
        } else {
            if !self.live_reasoning.is_empty() {
                let live = std::mem::take(&mut self.live_reasoning);
                append_bounded(
                    &mut self.hidden_reasoning,
                    &live,
                    MAX_HIDDEN_REASONING_BYTES,
                );
            }
            self.notice = Some("Thinking hidden (Ctrl-O to show)".to_owned());
        }
    }

    fn clear_live(&mut self) {
        self.markdown.clear();
        self.live_reasoning.clear();
        self.hidden_reasoning.clear();
        self.tool_preview.clear();
        self.emitted_reasoning = false;
        self.is_thinking = false;
    }

    fn flush_reasoning(&mut self) {
        self.is_thinking = false;
        self.hidden_reasoning.clear();
        if !self.live_reasoning.is_empty() {
            let tail = std::mem::take(&mut self.live_reasoning);
            self.scrollback
                .push(Line::styled(tail, self.theme.reasoning));
        }
        if self.emitted_reasoning {
            self.scrollback.push(Line::raw(String::new()));
            self.emitted_reasoning = false;
        }
    }

    fn flush_text(&mut self) {
        self.finish_markdown(None);
    }

    fn finish_markdown(&mut self, fallback: Option<&str>) {
        if self.sync_markdown_width() {
            self.markdown.rerender_live(&self.markdown_style);
        }
        if self
            .markdown
            .finish(fallback, &self.markdown_style, &mut self.scrollback)
        {
            self.scrollback.push(Line::raw(String::new()));
        }
    }

    /// Desired inline viewport height for `width`, growing with live streaming
    /// preview lines (up to [`MAX_LIVE_ROWS`]) and editor lines (up to
    /// [`MAX_EDITOR_ROWS`]).
    pub fn viewport_height(&self, width: u16) -> u16 {
        if width > 0 {
            self.width.set(width);
        }
        if let Some(picker) = &self.picker {
            return FIXED_CHROME_ROWS + picker.visible_rows();
        }
        FIXED_CHROME_ROWS
            + self.live_rows(width)
            + self.todo_lines().len() as u16
            + self.editor_rows(width)
            + self.completion.visible_rows()
    }

    /// The plan: a title with the progress, then the items around the one
    /// in progress, and how many more there are.
    fn todo_lines(&self) -> Vec<Line<'static>> {
        if self.todos.is_empty() {
            return Vec::new();
        }
        let total = self.todos.len();
        let done = self
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        let mut lines = vec![Line::from(vec![
            Span::styled("Plan", self.theme.prompt),
            Span::styled(format!(" {done}/{total}"), self.theme.muted),
        ])];

        // From just before the first unfinished item, so what was just done
        // and what comes next stay in view.
        let focus = self
            .todos
            .iter()
            .position(|t| t.status != TodoStatus::Completed)
            .unwrap_or(0);
        let start = focus
            .saturating_sub(1)
            .min(total.saturating_sub(MAX_TODO_ROWS));
        for todo in self.todos.iter().skip(start).take(MAX_TODO_ROWS) {
            let (mark, mark_style, style) = match todo.status {
                TodoStatus::Completed => ("✓", self.theme.todo_done, self.theme.todo_done),
                TodoStatus::InProgress => ("▸", self.theme.todo_current, self.theme.todo_current),
                TodoStatus::Pending => ("○", self.theme.muted, Style::default()),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(mark, mark_style.remove_modifier(Modifier::CROSSED_OUT)),
                Span::raw(" "),
                Span::styled(summary(&todo.content), style),
            ]));
        }
        let hidden = total - total.min(MAX_TODO_ROWS);
        if hidden > 0 {
            lines.push(Line::styled(
                format!("  … +{hidden} more"),
                self.theme.muted,
            ));
        }
        lines
    }

    fn live_rows(&self, width: u16) -> u16 {
        let width = (width as usize).max(1);
        let count = if !self.markdown.live_lines().is_empty() {
            wrapped_line_count(self.markdown.live_lines(), width)
        } else if !self.tool_preview.is_empty() {
            let lines = self.tool_preview.render_lines(self.theme.muted);
            wrapped_line_count(&lines, width)
        } else {
            1
        };
        (count as u16).clamp(1, MAX_LIVE_ROWS)
    }

    fn editor_rows(&self, width: u16) -> u16 {
        let text_w = (width as usize).saturating_sub(2).max(1);
        let layout = layout_editor(&self.input, text_w);
        (layout.rows.len() as u16).clamp(1, MAX_EDITOR_ROWS)
    }

    fn wrapped_live_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(1);
        if !self.markdown.live_lines().is_empty() {
            wrap_lines(self.markdown.live_lines(), width)
        } else if !self.tool_preview.is_empty() {
            let lines = self.tool_preview.render_lines(self.theme.muted);
            wrap_lines(&lines, width)
        } else if self.show_thinking && !self.live_reasoning.is_empty() {
            let line = Line::styled(self.live_reasoning.clone(), self.theme.reasoning);
            wrap_lines(&[line], width)
        } else {
            Vec::new()
        }
    }

    /// Draw the pi-style inline viewport: multi-line live preview,
    /// spinner/notice, framed multiline editor with cursor, and footer.
    /// Where the last [`App::render`] placed the visible cursor, if anywhere.
    pub fn cursor_position(&self) -> Option<Position> {
        self.cursor.get()
    }

    fn set_cursor(&self, frame: &mut Frame<'_>, position: Position) {
        self.cursor.set(Some(position));
        frame.set_cursor_position(position);
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        self.cursor.set(None);
        let area = frame.area();
        if area.width > 0 {
            self.width.set(area.width);
        }
        if let Some(picker) = &self.picker {
            let list_h = area.height.saturating_sub(FIXED_CHROME_ROWS).max(1);
            let [header_area, top_rule, list_area, bottom_rule, footer_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(list_h),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(area);
            frame.render_widget(
                Paragraph::new(picker.header_line(self.theme, area.width as usize)),
                header_area,
            );
            self.render_rule(frame, top_rule);
            picker.render(frame, list_area, self.theme);
            self.render_rule(frame, bottom_rule);
            self.render_footer(frame, footer_area);
            return;
        }
        let live_lines = self.wrapped_live_lines(area.width as usize);
        let todo_lines = self.todo_lines();
        let completion_h = self
            .completion
            .visible_rows()
            .min(area.height.saturating_sub(FIXED_CHROME_ROWS + 2));
        let todo_h = (todo_lines.len() as u16).min(
            area.height
                .saturating_sub(FIXED_CHROME_ROWS + 2 + completion_h),
        );
        let max_live = area
            .height
            .saturating_sub(FIXED_CHROME_ROWS + completion_h + todo_h + 1)
            .clamp(1, MAX_LIVE_ROWS);
        let live_h = (live_lines.len() as u16).clamp(1, max_live);
        let editor_h = area
            .height
            .saturating_sub(FIXED_CHROME_ROWS + live_h + todo_h + completion_h)
            .max(1);
        let [live_area, spinner_area, todo_area, top_rule, editor_area, bottom_rule, completion_area, footer_area] =
            Layout::vertical([
                Constraint::Length(live_h),
                Constraint::Length(1),
                Constraint::Length(todo_h),
                Constraint::Length(1),
                Constraint::Length(editor_h),
                Constraint::Length(1),
                Constraint::Length(completion_h),
                Constraint::Fill(1),
            ])
            .areas(area);

        self.render_live(frame, live_area, live_lines);
        self.render_spinner(frame, spinner_area);
        frame.render_widget(Paragraph::new(todo_lines), todo_area);
        self.render_rule(frame, top_rule);
        self.render_editor(frame, editor_area);
        self.render_rule(frame, bottom_rule);
        self.completion.render(frame, completion_area, self.theme);
        self.render_footer(frame, footer_area);
    }

    fn render_live(&self, frame: &mut Frame<'_>, area: Rect, mut lines: Vec<Line<'static>>) {
        if area.height == 0 || area.width == 0 || lines.is_empty() {
            return;
        }
        let visible = area.height as usize;
        if lines.len() > visible {
            lines.drain(..lines.len() - visible);
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_spinner(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let line = if self.busy {
            let spin = SPINNER[self.spinner];
            let elapsed = self
                .activity_started
                .map(|t| format_elapsed(t.elapsed().as_secs()))
                .filter(|s| !s.is_empty())
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            let label = match &self.active_tool {
                Some(tool) => format!("{spin} Running {}…{elapsed}  (esc to cancel)", tool.name),
                None if !self.show_thinking => {
                    format!("{spin} Thinking…{elapsed}  (Ctrl-O to show · esc to cancel)")
                }
                None if self.is_thinking => {
                    format!("{spin} Thinking…{elapsed}  (Ctrl-O to hide · esc to cancel)")
                }
                None => format!("{spin} Working…{elapsed}  (Ctrl-O to hide · esc to cancel)"),
            };
            let label = match self.queued {
                0 => label,
                n => format!("{label} · {n} queued"),
            };
            Line::styled(label, self.theme.muted)
        } else if let Some(notice) = &self.notice {
            Line::styled(notice.as_str(), self.theme.muted)
        } else {
            return;
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_rule(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let style = if self.busy {
            self.theme.border_busy
        } else {
            self.theme.border_idle
        };
        let rule = "─".repeat(area.width as usize);
        frame.render_widget(Paragraph::new(Line::styled(rule, style)), area);
    }

    fn render_editor(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let visible_rows = area.height as usize;
        let prefix_w = 2usize;
        let text_w = (area.width as usize).saturating_sub(prefix_w).max(1);

        if self.input.is_empty() {
            let hint = if self.busy {
                "Type a message to send while the agent works…"
            } else {
                "Type a message… (/help for commands)"
            };
            let line = Line::from(vec![
                Span::styled("> ", self.theme.prompt),
                Span::styled(hint, self.theme.muted),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            self.set_cursor(
                frame,
                Position::new(area.x.saturating_add(prefix_w as u16), area.y),
            );
            return;
        }

        let layout = layout_editor(&self.input, text_w);
        let scroll_row = layout
            .cursor_row
            .saturating_sub(visible_rows.saturating_sub(1));

        let mut rendered = Vec::with_capacity(visible_rows);
        for (idx, row_text) in layout
            .rows
            .iter()
            .enumerate()
            .skip(scroll_row)
            .take(visible_rows)
        {
            let prefix = if idx == 0 {
                Span::styled("> ", self.theme.prompt)
            } else {
                Span::styled("  ", self.theme.prompt)
            };
            rendered.push(Line::from(vec![prefix, Span::raw(row_text.as_str())]));
        }
        frame.render_widget(Paragraph::new(rendered), area);

        let x = area
            .x
            .saturating_add(prefix_w as u16)
            .saturating_add(layout.cursor_col as u16)
            .min(area.right().saturating_sub(1));
        let y = area
            .y
            .saturating_add(layout.cursor_row.saturating_sub(scroll_row) as u16)
            .min(area.bottom().saturating_sub(1));
        self.set_cursor(frame, Position::new(x, y));
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let width = area.width as usize;
        let model_label = format!("{} ({})", self.model, self.thinking);
        let right = format!("{} · {}", self.status.sandbox, self.status.folder);
        // Unknown before the first request of the conversation.
        let context = match self.context {
            0 => String::new(),
            tokens => format!(" · {} context", ScaledTokens(tokens)),
        };
        let left_w = model_label.width() + context.width();
        let gap = width.saturating_sub(left_w + right.width()).max(2);

        let line = Line::from(vec![
            Span::styled(model_label, self.theme.thinking_model(self.thinking)),
            Span::styled(context, self.theme.status_bar),
            Span::raw(" ".repeat(gap)),
            Span::styled(right, self.theme.status_bar),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// Append `delta` to `live`, flushing every `\n`-terminated line and every
/// soft-wrapped row exceeding `width` columns into `scrollback` with `style`.
fn stream_reasoning(
    live: &mut String,
    delta: &str,
    width: usize,
    style: Style,
    scrollback: &mut Vec<Line<'static>>,
) {
    let width = width.max(1);
    live.push_str(delta);

    while let Some(nl) = live.find('\n') {
        let line = live[..nl].strip_suffix('\r').unwrap_or(&live[..nl]);
        push_wrapped_str(line, width, style, scrollback);
        live.drain(..=nl);
    }

    while let Some((row_end, next_start)) = wrap_split(live, width) {
        scrollback.push(Line::styled(live[..row_end].to_owned(), style));
        live.drain(..next_start);
    }
}

fn push_wrapped_str(
    mut line: &str,
    width: usize,
    style: Style,
    scrollback: &mut Vec<Line<'static>>,
) {
    while let Some((row_end, next_start)) = wrap_split(line, width) {
        scrollback.push(Line::styled(line[..row_end].to_owned(), style));
        line = &line[next_start..];
    }
    scrollback.push(Line::styled(line.to_owned(), style));
}

/// If `text` exceeds `width` display columns, return `(row_end, next_start)`
/// splitting at the last whitespace break within `width` (or at the character
/// boundary reaching `width` when a single word exceeds `width`).
fn wrap_split(text: &str, width: usize) -> Option<(usize, usize)> {
    let mut col = 0usize;
    let mut has_non_ws = false;
    let mut last_break: Option<(usize, usize)> = None;

    for (idx, ch) in text.char_indices() {
        let ch_w = char_width(ch);
        if col > 0 && col + ch_w > width {
            if ch.is_whitespace() && has_non_ws {
                let row_end = last_break
                    .filter(|&(_, prev_next)| prev_next == idx)
                    .map_or(idx, |(prev_end, _)| prev_end);
                let ws_len: usize = text[idx..]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(char::len_utf8)
                    .sum();
                return Some((row_end, idx + ws_len));
            }
            if let Some(brk) = last_break {
                return Some(brk);
            }
            return Some((idx, idx));
        }
        col += ch_w;
        if ch.is_whitespace() {
            if has_non_ws {
                let next = idx + ch.len_utf8();
                let row_end = last_break
                    .filter(|&(_, prev_next)| prev_next == idx)
                    .map_or(idx, |(prev_end, _)| prev_end);
                last_break = Some((row_end, next));
            }
        } else {
            has_non_ws = true;
        }
        if col > width {
            let next = idx + ch.len_utf8();
            return Some((next, next));
        }
    }
    None
}

fn append_bounded(buf: &mut String, delta: &str, max_bytes: usize) {
    buf.push_str(delta);
    if buf.len() > max_bytes {
        let cut = buf.len() - max_bytes;
        let boundary = buf.ceil_char_boundary(cut);
        buf.drain(..boundary);
    }
}

fn parse_slash(input: &str) -> Option<Result<SlashCommand, String>> {
    if !input.starts_with('/') {
        return None;
    }
    let mut parts = input.splitn(2, char::is_whitespace);
    let cmd = parts.next()?;
    let arg = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(match cmd.to_ascii_lowercase().as_str() {
        "/model" => Ok(SlashCommand::Model(arg)),
        "/thinking" => Ok(SlashCommand::Thinking(arg)),
        "/resume" => Ok(SlashCommand::Resume(arg)),
        "/compact" => Ok(SlashCommand::Compact(arg)),
        "/retry" => Ok(SlashCommand::Retry),
        "/status" => Ok(SlashCommand::Status),
        "/output" => Ok(SlashCommand::Output),
        "/copy" => Ok(SlashCommand::Copy),
        "/new" => Ok(SlashCommand::New),
        "/quit" | "/exit" => Ok(SlashCommand::Quit),
        "/help" => Ok(SlashCommand::Help),
        _ => Err(cmd.to_owned()),
    })
}

fn format_tool_call(name: &str, args: &str, theme: Theme) -> Line<'static> {
    let parsed: Option<Value> = serde_json::from_str(args).ok();
    let detail = parsed.as_ref().and_then(|v| match name {
        "read" => {
            let path = v["path"].as_str()?;
            match v["offset"].as_u64() {
                Some(offset) => Some(format!("{path}:{offset}")),
                None => Some(path.to_owned()),
            }
        }
        "write" | "edit" => v["path"].as_str().map(str::to_owned),
        "bash" => v["command"].as_str().map(summary),
        "grep" => {
            let pat = v["pattern"].as_str()?;
            match v["path"].as_str() {
                Some(path) if !path.is_empty() => Some(format!("{pat} {path}")),
                _ => Some(pat.to_owned()),
            }
        }
        "find" => {
            let pat = v["pattern"].as_str()?;
            match v["path"].as_str() {
                Some(path) if !path.is_empty() => Some(format!("{pat} {path}")),
                _ => Some(pat.to_owned()),
            }
        }
        "ls" => Some(v["path"].as_str().unwrap_or(".").to_owned()),
        "ask_user" => v["question"].as_str().map(summary),
        "web_fetch" => {
            let url = v["url"].as_str()?;
            match v["offset"].as_u64() {
                Some(offset) => Some(format!("{url} @{offset}")),
                None => Some(url.to_owned()),
            }
        }
        _ => None,
    });
    let detail = detail.unwrap_or_else(|| summary(args));
    Line::from(vec![
        Span::styled(name.to_owned(), theme.tool_name),
        Span::raw(" "),
        Span::styled(detail, theme.tool_arg),
    ])
}

fn format_tool_result(
    tool: Option<&str>,
    content: &str,
    details: Option<&ToolDetails>,
    theme: Theme,
    out: &mut Vec<Line<'static>>,
) {
    let parsed: Option<Value> = serde_json::from_str(content).ok();
    if let Some(err) = parsed.as_ref().and_then(|v| v["error"].as_str()) {
        out.push(Line::styled(format!("  error: {err}"), theme.error));
        return;
    }

    if let Some(ToolDetails::Edit(diff)) = details {
        render_diff(diff, MAX_DIFF_PREVIEW_LINES, theme, out);
        return;
    }

    let Some(v) = parsed else {
        out.push(Line::styled(format!("  {}", summary(content)), theme.muted));
        return;
    };

    match tool {
        Some("bash") => {
            if let Some(output) = v["output"].as_str() {
                let mut lines = output.lines();
                for line in lines.by_ref().take(MAX_BASH_PREVIEW_LINES) {
                    out.push(tool_line(line, theme.muted));
                }
                let extra = lines.count();
                if extra > 0 {
                    out.push(Line::styled(
                        format!("  … +{extra} more lines"),
                        theme.muted,
                    ));
                }
            }
            if let Some(code) = v["exit_code"].as_i64().filter(|&c| c != 0) {
                out.push(Line::styled(format!("  exit {code}"), theme.error));
            }
            if v["timed_out"].as_bool() == Some(true) {
                out.push(Line::styled("  timed out", theme.error));
            }
            if let Some(path) = v["output_file"].as_str() {
                out.push(Line::styled(format!("  full output: {path}"), theme.muted));
            }
        }
        Some("read") => {
            let total = v["total_lines"].as_u64().unwrap_or(0);
            let suffix = truncated_suffix(&v);
            out.push(Line::styled(
                format!("  {total} lines{suffix}"),
                theme.muted,
            ));
        }
        Some("write") => {
            let bytes = v["bytes_written"].as_u64().unwrap_or(0);
            out.push(Line::styled(format!("  wrote {bytes} bytes"), theme.muted));
        }
        Some("ls") => {
            let count = v["entries"].as_array().map_or(0, Vec::len);
            out.push(Line::styled(format!("  {count} entries"), theme.muted));
        }
        Some("find") => {
            let total = v["total"].as_u64().unwrap_or(0);
            out.push(Line::styled(format!("  {total} files"), theme.muted));
        }
        Some("grep") => {
            let matches = v["matches"].as_u64().unwrap_or(0);
            let suffix = truncated_suffix(&v);
            out.push(Line::styled(
                format!("  {matches} matches{suffix}"),
                theme.muted,
            ));
        }
        Some("web_fetch") => {
            let total = v["total_bytes"].as_u64().unwrap_or(0);
            let suffix = truncated_suffix(&v);
            out.push(Line::styled(
                format!("  {total} bytes{suffix}"),
                theme.muted,
            ));
        }
        Some("ask_user") => {
            let answer = v["answer"].as_str().unwrap_or_default();
            out.push(Line::styled(
                format!("  answer: {}", summary(answer)),
                theme.muted,
            ));
        }
        _ => {
            out.push(Line::styled(format!("  {}", summary(content)), theme.muted));
        }
    }
}

fn truncated_suffix(v: &Value) -> &'static str {
    if v["truncated"].as_bool() == Some(true) {
        " (truncated)"
    } else {
        ""
    }
}

/// The whole output of `run`: its call, then the full diff or the text the
/// tool returned.
fn format_full_output(run: &ToolRun, theme: Theme, out: &mut Vec<Line<'static>>) {
    if let Some(call) = &run.call {
        out.push(format_tool_call(&call.name, &call.args, theme));
    }
    if let Some(ToolDetails::Edit(diff)) = &run.details {
        render_diff(diff, usize::MAX, theme, out);
        return;
    }
    let parsed: Option<Value> = serde_json::from_str(&run.content).ok();
    let style = match &parsed {
        Some(v) if v.get("error").is_some() => theme.error,
        _ => theme.muted,
    };
    let text = parsed
        .as_ref()
        .and_then(output_text)
        .unwrap_or(Cow::Borrowed(&run.content));
    out.extend(text.lines().map(|line| tool_line(line, style)));
}

/// The text a tool result carries: its error, output or content, its
/// entries or paths one per line, or else the whole result as indented JSON.
fn output_text(v: &Value) -> Option<Cow<'_, str>> {
    if let Some(text) = ["error", "output", "content"]
        .iter()
        .find_map(|k| v[k].as_str())
    {
        return Some(Cow::Borrowed(text));
    }
    if let Some(items) = ["entries", "paths"].iter().find_map(|k| v[k].as_array()) {
        let items: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
        return Some(Cow::Owned(items.join("\n")));
    }
    serde_json::to_string_pretty(v).ok().map(Cow::Owned)
}

/// Show up to `max_lines` lines of `diff`, each with its line number: the
/// old one for removed lines, the new one otherwise.
fn render_diff(diff: &Diff, max_lines: usize, theme: Theme, out: &mut Vec<Line<'static>>) {
    let last_line = diff
        .hunks
        .iter()
        .map(|h| (h.old_start + h.old_lines).max(h.new_start + h.new_lines) - 1)
        .max()
        .unwrap_or(0);
    let width = last_line.to_string().len();
    let mut emitted = 0usize;
    let mut omitted = 0usize;
    let mut push = |line: Line<'static>| {
        if emitted < max_lines {
            out.push(line);
            emitted += 1;
        } else {
            omitted += 1;
        }
    };
    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            push(Line::styled(format!("  {:>width$}", "⋮"), theme.diff_hunk));
        }
        let mut old_line = hunk.old_start;
        let mut new_line = hunk.new_start;
        for line in &hunk.lines {
            let (number, sign, text, style) = match line {
                DiffLine::Context(s) => {
                    old_line += 1;
                    (post_increment(&mut new_line), ' ', s, theme.muted)
                }
                DiffLine::Removed(s) => (post_increment(&mut old_line), '-', s, theme.diff_remove),
                DiffLine::Added(s) => (post_increment(&mut new_line), '+', s, theme.diff_add),
            };
            push(Line::from(vec![
                Span::styled(format!("  {number:>width$} "), theme.muted),
                Span::styled(format!("{sign}{}", expand_tabs(text)), style),
            ]));
        }
    }
    if omitted > 0 {
        out.push(Line::styled(
            format!("  … +{omitted} more diff lines"),
            theme.muted,
        ));
    } else if diff.truncated {
        out.push(Line::styled("  … diff truncated", theme.muted));
    }
}

/// Return `n`, then add one to it.
fn post_increment(n: &mut usize) -> usize {
    *n += 1;
    *n - 1
}

/// Columns between the tab stops of tool output.
const TAB_WIDTH: usize = 4;

/// A line of tool output, indented under its call.
fn tool_line(text: &str, style: Style) -> Line<'static> {
    Line::styled(format!("  {}", expand_tabs(text)), style)
}

/// `text` with its tabs turned into spaces, as the terminal drops them.
fn expand_tabs(text: &str) -> Cow<'_, str> {
    if !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + TAB_WIDTH);
    let mut col = 0;
    for ch in text.chars() {
        if ch == '\t' {
            let spaces = TAB_WIDTH - col % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', spaces));
            col += spaces;
        } else {
            out.push(ch);
            col += char_width(ch);
        }
    }
    Cow::Owned(out)
}

/// A tool call, with its arguments as JSON text.
struct ToolCall {
    name: String,
    args: String,
}

/// A finished tool call and its result.
struct ToolRun {
    /// Unknown for a restored result whose call is missing.
    call: Option<ToolCall>,
    content: String,
    details: Option<ToolDetails>,
}

/// Bounded ring buffer of live stdout/stderr lines from a running tool,
/// handling `\r` progress-bar overwrites without unbounded growth.
#[derive(Default)]
struct ToolPreview {
    lines: VecDeque<String>,
    current: String,
    cr_pending: bool,
}

impl ToolPreview {
    fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.current.is_empty()
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.current.clear();
        self.cr_pending = false;
    }

    fn push(&mut self, chunk: &str) {
        for ch in chunk.chars() {
            match ch {
                '\n' => {
                    self.cr_pending = false;
                    let line = std::mem::take(&mut self.current);
                    if self.lines.len() >= MAX_LIVE_ROWS as usize {
                        self.lines.pop_front();
                    }
                    self.lines.push_back(line);
                }
                '\r' => self.cr_pending = true,
                _ => {
                    if self.cr_pending {
                        self.current.clear();
                        self.cr_pending = false;
                    }
                    if self.current.len() < MAX_TOOL_PREVIEW_LINE_BYTES {
                        self.current.push(ch);
                    }
                }
            }
        }
    }

    fn render_lines(&self, style: Style) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(self.lines.len() + usize::from(!self.current.is_empty()));
        for line in &self.lines {
            out.push(tool_line(line, style));
        }
        if !self.current.is_empty() {
            out.push(tool_line(&self.current, style));
        }
        out
    }
}

fn format_elapsed(secs: u64) -> String {
    if secs == 0 {
        String::new()
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

struct ScaledTokens(u64);

impl std::fmt::Display for ScaledTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (tenth_unit, suffix) = if self.0 >= 1_000_000 {
            (100_000, 'M')
        } else if self.0 >= 1_000 {
            (100, 'K')
        } else {
            return write!(f, "{}", self.0);
        };
        let tenths = self.0 / tenth_unit;
        let (whole, frac) = (tenths / 10, tenths % 10);
        if frac == 0 {
            write!(f, "{whole}{suffix}")
        } else {
            write!(f, "{whole}.{frac}{suffix}")
        }
    }
}

struct EditorLayout {
    rows: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

/// Soft-wrap logical editor lines to `width` display columns and map the
/// cursor to its visual `(row, col)`.
fn layout_editor(input: &Input, width: usize) -> EditorLayout {
    let (target_row, target_col) = input.cursor_pos();
    let mut rows = Vec::new();
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;

    for (lrow, line) in input.lines().into_iter().enumerate() {
        let mut cur = String::new();
        let mut col = 0usize;
        let mut logical_col = 0usize;
        let mut cursor_set = false;

        if lrow == target_row && target_col == 0 {
            cursor_row = rows.len();
            cursor_col = 0;
            cursor_set = true;
        }

        for ch in line.chars() {
            let ch_w = char_width(ch);
            if col > 0 && col + ch_w > width {
                rows.push(std::mem::take(&mut cur));
                col = 0;
            }
            cur.push(ch);
            col += ch_w;
            logical_col += ch_w;

            if lrow == target_row && !cursor_set && logical_col >= target_col {
                if col >= width {
                    rows.push(std::mem::take(&mut cur));
                    col = 0;
                    cursor_row = rows.len();
                    cursor_col = 0;
                } else {
                    cursor_row = rows.len();
                    cursor_col = col;
                }
                cursor_set = true;
            }
        }
        rows.push(cur);
    }

    EditorLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// The line telling the user the final text of the model is not whole.
pub const TRUNCATED_NOTICE: &str = "[reply cut at the token limit of the model]";

/// The line telling the user a failed model request is retried.
pub fn retry_notice(attempt: u32, delay: Duration, reason: &str) -> String {
    format!("[{reason} · retry {attempt} in {delay:.0?}]")
}

/// The line telling the user old tool outputs were dropped to fit the
/// context window.
pub fn pruned_notice(count: usize) -> String {
    format!("[request too large: removed {count} old tool outputs]")
}

/// First line of `text`, cut to [`SUMMARY_LEN`] characters.
pub fn summary(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(SUMMARY_LEN) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

/// `text` without control characters but newlines and tabs, to print it
/// as it is: text from the model or from commands could otherwise send
/// escape sequences to the terminal, such as one that sets the clipboard.
pub fn printable(text: &str) -> Cow<'_, str> {
    let control = |c: char| c.is_control() && c != '\n' && c != '\t';
    if text.contains(control) {
        Cow::Owned(text.chars().filter(|&c| !control(c)).collect())
    } else {
        Cow::Borrowed(text)
    }
}
