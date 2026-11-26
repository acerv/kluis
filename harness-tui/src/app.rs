// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use genai::chat::{ChatMessage, ChatRole, ContentPart};
use harness_core::tools::{is_read_only, Diff, Image, Todo, TodoStatus, TodoTool, TODO_TOOL};
use harness_core::{Command, ModelInfo, SessionSummary, Skill, Thinking, TokenUsage, ToolDetails};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde::Deserialize;
use serde_json::Value;
use tui_markdown::CodeTheme;
use unicode_width::UnicodeWidthStr;

use crate::changes::{ChangesUpdate, Undo, UndoPlan};
use crate::complete::CommandCompletion;
use crate::diff_view::DiffView;
use crate::input::Input;
use crate::markdown::{render_markdown, MarkdownStream, MarkdownStyle};
use crate::picker::{Picker, PickerKind, PickerOutcome};
use crate::style::Theme;
use crate::text::{char_width, truncate_width, wrap_lines, wrapped_line_count};
use crate::transcript::Entry;
use crate::Status;

mod keys;
mod render;
mod tool_view;
mod update;

pub(crate) use tool_view::render_diff;
pub(crate) use tool_view::ToolRun;
use tool_view::{
    format_elapsed, format_tool_call, format_tool_run, ToolCall, ToolPreview, ToolReply,
};
pub use update::AgentUpdate;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The terminal title when idle, and the start of it when busy.
const TITLE: &str = "kl";

/// Maximum width of the activity in the terminal title.
const MAX_TITLE_WIDTH: usize = 40;

/// How long a prompt must run for its end, or a question, to ring the bell.
const LONG_PROMPT: Duration = Duration::from_secs(10);

/// Maximum characters shown in a single-line summary.
const SUMMARY_LEN: usize = 120;

/// Maximum visible rows in the live streaming preview above the spinner.
const MAX_LIVE_ROWS: u16 = 6;

/// How full the context gets, in percent of the size it is compacted at,
/// before the footer warns.
const NEAR_COMPACT_PERCENT: u64 = 80;

/// Maximum `todo` items shown above the editor.
const MAX_TODO_ROWS: usize = 8;

/// Maximum queued messages shown above the editor.
const MAX_QUEUED_ROWS: usize = 3;

/// Maximum visible rows inside the framed multiline editor before scrolling.
const MAX_EDITOR_ROWS: u16 = 8;

/// Columns the terminal needs to show the `/diff` panel.
pub const MIN_SPLIT_WIDTH: u16 = 60;

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

/// Interactive slash commands handled locally by the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Model(Option<String>),
    Thinking(Option<String>),
    Resume(Option<String>),
    Compact(Option<String>),
    Retry,
    Status,
    Copy,
    Export(Option<String>),
    Fork,
    Undo,
    Remember(Option<String>),
    Diff,
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
    /// Plan mode switched on or off.
    SetPlan(bool),
    /// The user's answer to the question of the model.
    Answer(String),
    /// Paste the image of the clipboard into the prompt.
    PasteImage,
    Cancel,
    Quit,
}

/// Pure TUI state machine, decoupled from terminal I/O so it can be tested
/// directly with ratatui's `TestBackend`.
pub struct App {
    model: String,
    thinking: Thinking,
    /// Plan mode: the model plans and changes nothing.
    plan: bool,
    status: Status,
    theme: Theme,
    width: Cell<u16>,
    /// Where the last [`App::render`] placed the visible cursor.
    cursor: Cell<Option<Position>>,
    input: Input,
    busy: bool,
    /// Messages typed while busy, waiting to join the conversation, in
    /// the order they will join it.
    queued: Vec<String>,
    /// The images pasted into the prompt, with the words naming them there.
    images: Vec<(String, Image)>,
    /// Verbose output: tool results in full, and the thinking.
    verbose: bool,
    /// The transcript must be printed again, as the output mode changed.
    reprint: bool,
    is_thinking: bool,
    spinner: usize,
    activity_started: Option<Instant>,
    /// When the running prompt started.
    prompt_started: Option<Instant>,
    /// Ring the bell when a long prompt ends or asks a question.
    notify: bool,
    /// The bell is due at the next draw.
    bell: bool,
    active_tool: Option<ToolCall>,
    /// Bounded live stdout/stderr preview while a streaming tool runs.
    tool_preview: ToolPreview,
    /// Incremental Markdown stream for the current assistant turn.
    markdown: MarkdownStream,
    markdown_style: MarkdownStyle,
    /// Current incomplete streaming reasoning row (fits within `width`).
    live_reasoning: String,
    /// Whether any reasoning lines were emitted during the current turn.
    emitted_reasoning: bool,
    /// Cumulative token usage across the active conversation.
    usage: TokenUsage,
    /// Tokens the last request sent, as the provider counted them: the
    /// size of the conversation, or 0 before its first request.
    context: u64,
    /// The size the conversation is compacted at, if it is.
    compact_at: Option<u64>,
    /// Output tokens of the running prompt, its subagents too.
    prompt_output: u64,
    notice: Option<String>,
    picker: Option<Picker>,
    completion: CommandCompletion,
    /// The plan of the `todo` tool, shown above the editor.
    todos: Vec<Todo>,
    /// The changes `/diff` shows.
    diff: DiffView,
    scrollback: Vec<Entry>,
}

impl App {
    pub fn new(model: impl Into<String>, status: Status, theme: Theme) -> Self {
        Self {
            model: model.into(),
            thinking: Thinking::Default,
            plan: false,
            status,
            theme,
            width: Cell::new(DEFAULT_WIDTH),
            cursor: Cell::new(None),
            input: Input::default(),
            busy: false,
            queued: Vec::new(),
            images: Vec::new(),
            verbose: false,
            reprint: false,
            is_thinking: false,
            spinner: 0,
            activity_started: None,
            prompt_started: None,
            notify: false,
            bell: false,
            active_tool: None,
            tool_preview: ToolPreview::default(),
            markdown: MarkdownStream::default(),
            markdown_style: MarkdownStyle::new(theme.color, DEFAULT_WIDTH),
            live_reasoning: String::new(),
            emitted_reasoning: false,
            usage: TokenUsage::default(),
            context: 0,
            compact_at: None,
            prompt_output: 0,
            notice: None,
            picker: None,
            completion: CommandCompletion::default(),
            todos: Vec::new(),
            diff: DiffView::default(),
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
        // The call shows once answered: show the question meanwhile.
        for (i, line) in question.lines().enumerate() {
            let mark = if i == 0 { "? " } else { "  " };
            self.push_line(Line::from(vec![
                Span::styled(mark, self.theme.prompt),
                Span::raw(line.to_owned()),
            ]));
        }
        self.picker = Some(Picker::answers(options));
        self.ring_if_long();
    }

    /// Ring the bell when a long prompt ends or asks a question.
    pub fn set_notify(&mut self, notify: bool) {
        self.notify = notify;
    }

    /// Whether the bell is due, clearing it.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Make the bell due if the user may have stopped watching a prompt
    /// that has run for long.
    fn ring_if_long(&mut self) {
        self.bell |= self.notify
            && self
                .prompt_started
                .is_some_and(|t| t.elapsed() >= LONG_PROMPT);
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

    /// The terminal title: what the harness is doing, from the same state
    /// as the spinner.
    pub fn title(&self) -> String {
        let activity = if self.picker.as_ref().map(|p| p.kind()) == Some(PickerKind::Answer) {
            "Question".to_owned()
        } else if !self.busy {
            return TITLE.to_owned();
        } else if let Some(tool) = &self.active_tool {
            // The model names the tool: keep only printable characters.
            let name: String = tool.name.chars().filter(|c| !c.is_control()).collect();
            truncate_width(&format!("Running {name}"), MAX_TITLE_WIDTH)
        } else if self.is_thinking {
            "Thinking".to_owned()
        } else {
            "Working".to_owned()
        };
        format!("{TITLE}: {activity}")
    }

    /// Whether the output is verbose: tool results in full, and the
    /// thinking.
    pub fn verbose(&self) -> bool {
        self.verbose
    }

    /// Whether the transcript must be printed again, clearing it.
    pub(crate) fn take_reprint(&mut self) -> bool {
        std::mem::take(&mut self.reprint)
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

    /// Whether plan mode is on.
    pub fn plan(&self) -> bool {
        self.plan
    }

    pub fn set_thinking(&mut self, thinking: Thinking) {
        self.thinking = thinking;
    }

    /// Thinking levels the model takes, when its provider says.
    pub fn thinking_levels(&self) -> Option<&[Thinking]> {
        self.completion.thinking_levels()
    }

    /// Set the thinking levels the model takes, `None` when unknown.
    pub fn set_thinking_levels(&mut self, levels: Option<Vec<Thinking>>) {
        self.completion.set_thinking_levels(levels);
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
        self.push_line(Line::raw(String::new()));
        for row in 0..LOGO.len().max(info.len()) {
            let logo = LOGO.get(row).copied().unwrap_or_default();
            let logo_style = self.theme.logo.get(row).copied().unwrap_or_default();
            let Some(line) = info.get(row) else {
                self.push_line(Line::styled(logo.to_owned(), logo_style));
                continue;
            };
            let pad = " ".repeat(logo_w - logo.width() + LOGO_GAP);
            let style = if line.warn {
                self.theme.error
            } else {
                self.theme.muted
            };
            self.push_line(Line::from(vec![
                Span::styled(format!("{logo}{pad}"), logo_style),
                Span::styled(line.text.clone(), style),
            ]));
        }
        self.push_line(Line::raw(String::new()));
    }

    /// Push informational muted text into scrollback (one line per `\n`).
    pub fn push_info(&mut self, msg: impl AsRef<str>) {
        let text = msg.as_ref();
        if text.is_empty() {
            self.push_line(Line::raw(String::new()));
            return;
        }
        for line in text.lines() {
            self.push_line(Line::styled(line.to_owned(), self.theme.muted));
        }
    }

    /// Paste `image` into the prompt: its name goes where the cursor is.
    pub fn paste_image(&mut self, image: Image) {
        let name = format!("[Image #{}]", self.images.len() + 1);
        self.input.paste(&name);
        self.completion.update(self.input.text());
        self.images.push((name, image));
    }

    /// Take the pasted images, with their names, that the prompt `text`
    /// still names.
    pub fn take_images(&mut self, text: &str) -> Vec<(String, Image)> {
        let mut images = std::mem::take(&mut self.images);
        images.retain(|(name, _)| text.contains(name.as_str()));
        images
    }

    /// Forget the conversation and drop pending scrollback (e.g. on `/new`).
    pub fn reset_session(&mut self) {
        self.usage = TokenUsage::default();
        self.context = 0;
        self.clear_live();
        self.scrollback.clear();
        self.todos.clear();
        self.notice = None;
        if let Err(err) = self.diff.restart() {
            self.push_info(format!("[diff error: {err}]"));
        }
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

    /// Show the size of the conversation against `tokens`, the size it is
    /// compacted at, if any.
    pub fn set_compact_at(&mut self, tokens: Option<u64>) {
        self.compact_at = tokens;
    }

    /// Let `/<name>` run each of `commands` and ask for each of `skills`.
    pub fn set_custom(&mut self, commands: &[Command], skills: &[Skill]) {
        let commands = commands
            .iter()
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect();
        let skills = skills
            .iter()
            .map(|s| (s.name.clone(), s.description.clone()))
            .collect();
        self.completion.set_custom(commands, skills);
    }

    /// Offer the files of `root` when completing `@` in the editor.
    pub fn set_workspace(&mut self, root: PathBuf) {
        self.completion.set_root(root);
    }

    /// Take a snapshot of `root`, for `/diff` to show what the session
    /// changes there.
    pub fn watch_changes(&mut self, root: PathBuf) -> io::Result<()> {
        self.diff.watch(root)
    }

    /// Whether the `/diff` panel is open.
    pub fn is_diff_open(&self) -> bool {
        self.diff.is_open()
    }

    /// Open or close the `/diff` panel.
    pub fn toggle_diff(&mut self) {
        self.diff.toggle();
        if self.diff.is_open() && self.width.get() < MIN_SPLIT_WIDTH {
            self.notice = Some(format!(
                "/diff needs a terminal of {MIN_SPLIT_WIDTH} columns at least"
            ));
        }
    }

    /// Remember the files of the project as they are, before a prompt
    /// joins a conversation of `messages` messages, the last one told by
    /// `tag`, for `/undo`.
    pub async fn checkpoint(&self, messages: usize, tag: u64) {
        self.diff.checkpoint(messages, tag).await;
    }

    /// What [`Self::undo`] would do, `None` when there is nothing to undo.
    pub async fn undo_plan(&self) -> Option<UndoPlan> {
        self.diff.undo_plan().await
    }

    /// Put the files back as they were before the last prompt, as the
    /// plan `shown` says: when it no longer holds, nothing is done, and
    /// the plan as it is now is returned.
    pub async fn undo(&self, shown: UndoPlan) -> Option<Result<Undo, UndoPlan>> {
        self.diff.undo(shown).await
    }

    /// The messages and the tag the last checkpoint was taken with.
    pub async fn last_checkpoint(&self) -> Option<(usize, u64)> {
        self.diff.last_checkpoint().await
    }

    /// Forget the files as they were before the last prompt, which is
    /// gone, or before `all` of them, when the conversation is another one.
    pub fn forget_checkpoints(&self, all: bool) {
        self.diff.forget_checkpoints(all);
    }

    /// What the watcher of the changes found next; never, without one.
    pub async fn next_changes(&mut self) -> ChangesUpdate {
        self.diff.next().await
    }

    /// Show what the watcher of the changes found.
    pub fn apply_changes(&mut self, update: ChangesUpdate) {
        self.diff.apply(update);
    }

    /// Multi-line summary of the active model, session, context, and token
    /// usage shown by `/status`.
    pub fn status_text(&self, session: Option<&str>, messages: usize) -> String {
        use std::fmt::Write;

        let mut out = String::with_capacity(128);
        let _ = write!(out, "model:   {} ({})", self.model, self.thinking);
        if self.plan {
            out.push_str("\nmode:    plan");
        }
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
    /// unknown until [`App::set_context`] or its next request, with the
    /// changes its `edit` calls made, by call id.
    pub fn replay_history(&mut self, messages: &[ChatMessage], mut diffs: HashMap<String, Diff>) {
        self.usage = TokenUsage::default();
        self.context = 0;
        // The plan of the conversation shown until now is not of this one.
        self.todos.clear();
        self.sync_markdown_width();
        // The calls waiting for their result, by id: where they show, none
        // for a plan, and their name and arguments.
        let mut tool_calls: HashMap<&str, (Option<usize>, &str, &Value)> = HashMap::new();
        for msg in messages {
            match msg.role {
                // What the user typed, without the files and notes added.
                ChatRole::User => {
                    if let Some(text) = msg.content.first_text() {
                        self.push_user_message(text);
                    }
                }
                ChatRole::Assistant => {
                    for part in msg.content.parts() {
                        match part {
                            ContentPart::Text(text) if !text.is_empty() => {
                                let lines = render_markdown(text, &self.markdown_style);
                                self.push_lines(lines);
                                self.push_line(Line::raw(String::new()));
                            }
                            ContentPart::ToolCall(call) => {
                                // Shown as interrupted until its result.
                                let at = (call.fn_name != TODO_TOOL).then(|| {
                                    self.scrollback.push(Entry::Tool(Box::new(ToolRun {
                                        call: Some(ToolCall {
                                            name: call.fn_name.clone(),
                                            args: call.fn_arguments.to_string(),
                                        }),
                                        reply: None,
                                        elapsed: None,
                                    })));
                                    self.scrollback.len() - 1
                                });
                                tool_calls.insert(
                                    call.call_id.as_str(),
                                    (at, call.fn_name.as_str(), &call.fn_arguments),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                ChatRole::Tool => {
                    for resp in msg.content.tool_responses() {
                        let diff = diffs.remove(resp.call_id.as_str());
                        let reply =
                            ToolReply::new(resp.content.clone(), diff.map(ToolDetails::Edit));
                        let (at, name, args) = match tool_calls.remove(resp.call_id.as_str()) {
                            Some((at, name, args)) => (at, Some(name), Some(args)),
                            None => (None, None, None),
                        };
                        if name == Some(TODO_TOOL) && self.set_todos(&resp.content, None, args) {
                            continue;
                        }
                        let run = ToolRun {
                            call: name.zip(args).map(|(name, args)| ToolCall {
                                name: name.to_owned(),
                                args: args.to_string(),
                            }),
                            reply: Some(reply),
                            elapsed: None,
                        };
                        match at {
                            Some(at) => self.scrollback[at] = Entry::Tool(Box::new(run)),
                            None => self.scrollback.push(Entry::Tool(Box::new(run))),
                        }
                    }
                }
                ChatRole::System => {}
            }
        }
        self.drop_finished_plan();
    }

    /// Show the plan of a `todo` result above the editor. Returns `false`
    /// when the result holds no plan, as for an error.
    fn set_todos(
        &mut self,
        content: &str,
        details: Option<&ToolDetails>,
        call_args: Option<&Value>,
    ) -> bool {
        if let Some(ToolDetails::Todo(todos)) = details {
            self.todos = todos.clone();
            return true;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(content) {
            if parsed.get("error").is_none() {
                if let Some(plan) = call_args
                    .and_then(|args| TodoTool::deserialize(args).ok())
                    .or_else(|| TodoTool::deserialize(&parsed).ok())
                {
                    self.todos = plan.todos;
                    return true;
                }
            }
        }
        false
    }

    /// Hide the plan once all of it is done.
    fn drop_finished_plan(&mut self) {
        if self.todos.iter().all(|t| t.status == TodoStatus::Completed) {
            self.todos.clear();
        }
    }

    /// Queue `line` for the native terminal scrollback.
    fn push_line(&mut self, line: Line<'static>) {
        self.scrollback.push(Entry::Line(line));
    }

    fn push_lines(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        self.scrollback.extend(lines.into_iter().map(Entry::Line));
    }

    /// Take the entries queued for the native terminal scrollback.
    pub(crate) fn take_entries(&mut self) -> Vec<Entry> {
        std::mem::take(&mut self.scrollback)
    }

    /// Take the lines queued for the native terminal scrollback, as they
    /// print.
    pub fn take_scrollback(&mut self) -> Vec<Line<'static>> {
        let entries = self.take_entries();
        self.render_entries(&entries)
    }

    /// `entries` as they print.
    pub(crate) fn render_entries<'a>(
        &self,
        entries: impl IntoIterator<Item = &'a Entry>,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for entry in entries {
            self.render_entry(entry, &mut out);
        }
        out
    }

    /// The last entries, as they print, up to about `max` lines.
    pub(crate) fn render_tail<'a>(
        &self,
        entries: impl DoubleEndedIterator<Item = &'a Entry>,
        max: usize,
    ) -> Vec<Line<'static>> {
        let mut parts = Vec::new();
        let mut count = 0;
        for entry in entries.rev() {
            if count >= max {
                break;
            }
            let mut lines = Vec::new();
            self.render_entry(entry, &mut lines);
            count += lines.len();
            parts.push(lines);
        }
        parts.into_iter().rev().flatten().collect()
    }

    fn render_entry(&self, entry: &Entry, out: &mut Vec<Line<'static>>) {
        match entry {
            Entry::Line(line) => out.push(line.clone()),
            Entry::Reasoning(line) if self.verbose => out.push(line.clone()),
            Entry::Reasoning(_) => {}
            Entry::Tool(run) => format_tool_run(run, self.verbose, self.theme, out),
            Entry::Frozen(lines) => out.extend(lines.iter().cloned()),
        }
    }

    /// The short form of `run`, for an entry that keeps only that.
    pub(crate) fn freeze(&self, run: &ToolRun) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        format_tool_run(run, false, self.theme, &mut out);
        out
    }

    /// Advance the activity spinner while busy.
    pub fn tick(&mut self) {
        if self.busy {
            self.spinner = (self.spinner + 1) % SPINNER.len();
        }
        self.diff.tick(self.active_tool.is_some());
    }

    /// Override the elapsed time of the activity and of the prompt (useful
    /// for deterministic tests).
    pub fn set_elapsed(&mut self, elapsed: Duration) {
        self.activity_started = Instant::now().checked_sub(elapsed);
        self.prompt_started = self.activity_started;
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
        self.prompt_output = 0;
        self.spinner = 0;
        self.activity_started = Some(Instant::now());
        self.prompt_started = self.activity_started;
        self.active_tool = None;
        self.clear_live();
        self.completion.clear();
        self.completion.forget_files();
        self.notice = None;
    }

    fn push_user_message(&mut self, text: &str) {
        self.push_line(Line::raw(String::new()));
        for line in text.lines() {
            if self.theme.color {
                self.push_line(Line::styled(format!(" {line} "), self.theme.user));
            } else {
                self.push_line(Line::raw(format!("> {line}")));
            }
        }
        self.push_line(Line::raw(String::new()));
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
                self.emitted_reasoning = true;
                self.push_reasoning(&delta);
            }
            AgentUpdate::TextDelta(delta) => {
                self.flush_reasoning();
                self.sync_markdown_width();
                let mut lines = Vec::new();
                self.markdown
                    .push_delta(&delta, &self.markdown_style, &mut lines);
                self.push_lines(lines);
            }
            AgentUpdate::Text(text) => {
                self.flush_reasoning();
                self.finish_markdown(Some(&text));
            }
            AgentUpdate::TurnEnd(usage) => {
                self.usage += usage;
                self.prompt_output += usage.output;
                self.context = usage.prompt();
            }
            AgentUpdate::TaskUsage(usage) => {
                self.usage += usage;
                self.prompt_output += usage.output;
            }
            AgentUpdate::Retry {
                attempt,
                delay,
                reason,
            } => {
                // Keep what the failed attempt showed, marked as such.
                self.flush_reasoning();
                self.flush_text();
                self.push_line(Line::styled(
                    retry_notice(attempt, delay, &reason),
                    self.theme.muted,
                ));
            }
            AgentUpdate::ToolCall { name, args } => {
                self.flush_reasoning();
                self.flush_text();
                self.tool_preview.clear();
                self.activity_started = Some(Instant::now());
                // It shows once it ends, with how it ended.
                self.active_tool = Some(ToolCall { name, args });
            }
            AgentUpdate::ToolOutput(chunk) => {
                self.tool_preview.push(&chunk);
            }
            AgentUpdate::ToolResult { content, details } => {
                let call = self.active_tool.take();
                let elapsed = self.activity_started.map(|t| t.elapsed());
                self.tool_preview.clear();
                self.activity_started = Some(Instant::now());
                if call.as_ref().is_some_and(|c| !is_read_only(&c.name)) {
                    self.diff.refresh();
                }
                if call.as_ref().is_some_and(|c| c.name == TODO_TOOL)
                    && self.set_todos(&content, details.as_ref(), None)
                {
                    return;
                }
                let run = ToolRun {
                    call,
                    reply: Some(ToolReply::new(content, details)),
                    elapsed,
                };
                self.scrollback.push(Entry::Tool(Box::new(run)));
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
                if let Some(i) = self.queued.iter().position(|q| *q == text) {
                    self.queued.remove(i);
                }
                self.push_user_message(&text);
            }
        }
    }

    fn push_notice(&mut self, text: String, style: Style) {
        self.flush_reasoning();
        self.flush_text();
        self.push_line(Line::styled(text, style));
        self.push_line(Line::raw(String::new()));
    }

    /// Show the running tool call, if any, as one that never ended.
    fn interrupt_tool(&mut self) {
        if let Some(call) = self.active_tool.take() {
            self.scrollback.push(Entry::Tool(Box::new(ToolRun {
                call: Some(call),
                reply: None,
                elapsed: None,
            })));
        }
    }

    /// Leave the busy state when the prompt finishes or fails.
    pub fn finish_prompt(&mut self, outcome: Result<(), String>) {
        self.flush_reasoning();
        self.flush_text();
        self.interrupt_tool();
        self.ring_if_long();
        self.busy = false;
        self.queued.clear();
        self.activity_started = None;
        self.prompt_started = None;
        self.clear_live();
        self.drop_finished_plan();
        self.diff.refresh();
        if let Err(err) = outcome {
            self.push_line(Line::styled(format!("error: {err}"), self.theme.error));
            self.push_line(Line::raw(String::new()));
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
        self.interrupt_tool();
        self.busy = false;
        self.queued.clear();
        self.activity_started = None;
        self.prompt_started = None;
        self.clear_live();
        // The work was dropped; the model sends the plan again to go on.
        self.todos.clear();
        self.diff.refresh();
        self.push_line(Line::styled("[cancelled]", self.theme.muted));
        self.push_line(Line::raw(String::new()));
    }

    /// Switch between quiet and verbose output, printing the transcript
    /// again in the new mode.
    fn toggle_verbose(&mut self) {
        self.verbose = !self.verbose;
        self.reprint = true;
        self.notice = Some(if self.verbose {
            "Verbose: full tool output and thinking (Ctrl-O for quiet)".to_owned()
        } else {
            "Quiet: short tool output, no thinking (Ctrl-O for verbose)".to_owned()
        });
    }

    /// Stream `delta` of the thinking, committing its finished rows.
    fn push_reasoning(&mut self, delta: &str) {
        let mut lines = Vec::new();
        stream_reasoning(
            &mut self.live_reasoning,
            delta,
            self.width.get() as usize,
            self.theme.reasoning,
            &mut lines,
        );
        self.scrollback
            .extend(lines.into_iter().map(Entry::Reasoning));
    }

    fn clear_live(&mut self) {
        self.markdown.clear();
        self.live_reasoning.clear();
        self.tool_preview.clear();
        self.emitted_reasoning = false;
        self.is_thinking = false;
    }

    fn flush_reasoning(&mut self) {
        self.is_thinking = false;
        if !self.live_reasoning.is_empty() {
            let tail = std::mem::take(&mut self.live_reasoning);
            let line = Line::styled(tail, self.theme.reasoning);
            self.scrollback.push(Entry::Reasoning(line));
        }
        if self.emitted_reasoning {
            self.scrollback.push(Entry::Reasoning(Line::default()));
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
        let mut lines = Vec::new();
        if self
            .markdown
            .finish(fallback, &self.markdown_style, &mut lines)
        {
            lines.push(Line::default());
        }
        self.push_lines(lines);
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
        "/undo" => Ok(SlashCommand::Undo),
        "/status" => Ok(SlashCommand::Status),
        "/copy" => Ok(SlashCommand::Copy),
        "/export" => Ok(SlashCommand::Export(arg)),
        "/fork" => Ok(SlashCommand::Fork),
        "/remember" => Ok(SlashCommand::Remember(arg)),
        "/diff" => Ok(SlashCommand::Diff),
        "/new" => Ok(SlashCommand::New),
        "/quit" | "/exit" => Ok(SlashCommand::Quit),
        "/help" => Ok(SlashCommand::Help),
        _ => Err(cmd.to_owned()),
    })
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
