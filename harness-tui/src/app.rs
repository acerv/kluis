// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use genai::chat::{ChatMessage, ChatRole, ContentPart};
use harness_core::tools::{Diff, DiffLine};
use harness_core::{
    Event as AgentEvent, ModelInfo, SessionSummary, Thinking, TokenUsage, ToolDetails,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::complete::CommandCompletion;
use crate::input::Input;
use crate::markdown::{render_markdown, MarkdownStream};
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
    ToolCall {
        name: String,
        args: String,
    },
    ToolOutput(String),
    ToolResult {
        content: String,
        details: Option<ToolDetails>,
    },
}

impl AgentUpdate {
    /// Convert a borrowed [`harness_core::Event`] into an owned update.
    pub fn from_event(event: AgentEvent<'_>) -> Option<Self> {
        Some(match event {
            AgentEvent::TurnStart => Self::TurnStart,
            AgentEvent::ReasoningDelta(text) => Self::ReasoningDelta(text.to_owned()),
            AgentEvent::TextDelta(text) => Self::TextDelta(text.to_owned()),
            AgentEvent::Text(text) => Self::Text(text.to_owned()),
            AgentEvent::TurnEnd(usage) => Self::TurnEnd(TokenUsage::from_genai(usage?)),
            AgentEvent::ToolCall(call) => Self::ToolCall {
                name: call.fn_name.clone(),
                args: call.fn_arguments.to_string(),
            },
            AgentEvent::ToolOutput(chunk) => Self::ToolOutput(chunk.to_owned()),
            AgentEvent::ToolResult(resp, details) => Self::ToolResult {
                content: resp.content.clone(),
                details: details.cloned(),
            },
        })
    }
}

/// Interactive slash commands handled locally by the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Model(Option<String>),
    Thinking(Option<String>),
    Resume(Option<String>),
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
    show_thinking: bool,
    is_thinking: bool,
    spinner: usize,
    activity_started: Option<Instant>,
    active_tool: Option<String>,
    /// Bounded live stdout/stderr preview while a streaming tool runs.
    tool_preview: ToolPreview,
    /// Incremental Markdown stream for the current assistant turn.
    markdown: MarkdownStream,
    /// Current incomplete streaming reasoning row (fits within `width`).
    live_reasoning: String,
    /// Reasoning buffered while `show_thinking` is off so `Ctrl-O` can reveal
    /// the current turn's thinking in real time.
    hidden_reasoning: String,
    /// Whether any reasoning lines were emitted during the current turn.
    emitted_reasoning: bool,
    tokens: TokenUsage,
    notice: Option<String>,
    picker: Option<Picker>,
    completion: CommandCompletion,
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
            show_thinking: true,
            is_thinking: false,
            spinner: 0,
            activity_started: None,
            active_tool: None,
            tool_preview: ToolPreview::default(),
            markdown: MarkdownStream::default(),
            live_reasoning: String::new(),
            hidden_reasoning: String::new(),
            emitted_reasoning: false,
            tokens: TokenUsage::default(),
            notice: None,
            picker: None,
            completion: CommandCompletion::default(),
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

    /// Reset displayed token counters and drop pending scrollback (e.g. on `/new`).
    pub fn reset_session(&mut self) {
        self.set_tokens(TokenUsage::default());
        self.clear_live();
        self.scrollback.clear();
        self.notice = None;
    }

    /// Set displayed token counters (e.g. when restoring a persisted session).
    pub fn set_tokens(&mut self, usage: TokenUsage) {
        self.tokens = usage;
    }

    /// Replay a restored conversation history into scrollback.
    pub fn replay_history(&mut self, messages: &[ChatMessage]) {
        let mut tool_names: HashMap<&str, &str> = HashMap::new();
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
                                    .extend(render_markdown(text, self.theme.color));
                                self.scrollback.push(Line::raw(String::new()));
                            }
                            ContentPart::ToolCall(call) => {
                                tool_names.insert(call.call_id.as_str(), call.fn_name.as_str());
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
                        let tool = tool_names.get(resp.call_id.as_str()).copied();
                        format_tool_result(
                            tool,
                            &resp.content,
                            None,
                            self.theme,
                            &mut self.scrollback,
                        );
                        self.scrollback.push(Line::raw(String::new()));
                    }
                }
                ChatRole::System => {}
            }
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
        self.busy = true;
        self.spinner = 0;
        self.activity_started = Some(Instant::now());
        self.active_tool = None;
        self.clear_live();
        self.completion.clear();
        self.notice = None;
        self.push_user_message(text);
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
                self.markdown
                    .push_delta(&delta, self.theme.color, &mut self.scrollback);
            }
            AgentUpdate::Text(text) => {
                self.flush_reasoning();
                if self
                    .markdown
                    .finish(Some(&text), self.theme.color, &mut self.scrollback)
                {
                    self.scrollback.push(Line::raw(String::new()));
                }
            }
            AgentUpdate::TurnEnd(usage) => {
                self.tokens += usage;
            }
            AgentUpdate::ToolCall { name, args } => {
                self.flush_reasoning();
                self.flush_text();
                self.tool_preview.clear();
                self.active_tool = Some(name.clone());
                self.activity_started = Some(Instant::now());
                self.scrollback
                    .push(format_tool_call(&name, &args, self.theme));
            }
            AgentUpdate::ToolOutput(chunk) => {
                self.tool_preview.push(&chunk);
            }
            AgentUpdate::ToolResult { content, details } => {
                let tool_name = self.active_tool.take();
                self.tool_preview.clear();
                self.activity_started = Some(Instant::now());
                format_tool_result(
                    tool_name.as_deref(),
                    &content,
                    details.as_ref(),
                    self.theme,
                    &mut self.scrollback,
                );
                self.scrollback.push(Line::raw(String::new()));
            }
        }
    }

    /// Leave the busy state when the prompt finishes or fails.
    pub fn finish_prompt(&mut self, outcome: Result<(), String>) {
        self.flush_reasoning();
        self.flush_text();
        self.busy = false;
        self.activity_started = None;
        self.active_tool = None;
        self.clear_live();
        if let Err(err) = outcome {
            self.scrollback
                .push(Line::styled(format!("error: {err}"), self.theme.error));
            self.scrollback.push(Line::raw(String::new()));
        }
    }

    /// Leave the busy state after the user cancels an in-flight prompt.
    pub fn cancel_prompt(&mut self) {
        self.flush_reasoning();
        self.flush_text();
        self.busy = false;
        self.activity_started = None;
        self.active_tool = None;
        self.clear_live();
        self.scrollback
            .push(Line::styled("[cancelled]", self.theme.muted));
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Process a crossterm terminal event and return the next action.
    pub fn handle_event(&mut self, event: TermEvent) -> Action {
        match event {
            TermEvent::Resize(w, _) => {
                if w > 0 {
                    self.width.set(w);
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
            TermEvent::Paste(text) if !self.busy || self.picker.is_some() => {
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

        if self.busy {
            return Action::None;
        }

        if key.code == KeyCode::BackTab {
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
        if self
            .markdown
            .finish(None, self.theme.color, &mut self.scrollback)
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
            + self.editor_rows(width)
            + self.completion.visible_rows()
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
        let completion_h = self
            .completion
            .visible_rows()
            .min(area.height.saturating_sub(FIXED_CHROME_ROWS + 2));
        let max_live = area
            .height
            .saturating_sub(FIXED_CHROME_ROWS + completion_h + 1)
            .clamp(1, MAX_LIVE_ROWS);
        let live_h = (live_lines.len() as u16).clamp(1, max_live);
        let editor_h = area
            .height
            .saturating_sub(FIXED_CHROME_ROWS + live_h + completion_h)
            .max(1);
        let [live_area, spinner_area, top_rule, editor_area, bottom_rule, completion_area, footer_area] =
            Layout::vertical([
                Constraint::Length(live_h),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(editor_h),
                Constraint::Length(1),
                Constraint::Length(completion_h),
                Constraint::Fill(1),
            ])
            .areas(area);

        self.render_live(frame, live_area, live_lines);
        self.render_spinner(frame, spinner_area);
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
                Some(tool) => format!("{spin} Running {tool}…{elapsed}  (esc to cancel)"),
                None if !self.show_thinking => {
                    format!("{spin} Thinking…{elapsed}  (Ctrl-O to show · esc to cancel)")
                }
                None if self.is_thinking => {
                    format!("{spin} Thinking…{elapsed}  (Ctrl-O to hide · esc to cancel)")
                }
                None => format!("{spin} Working…{elapsed}  (Ctrl-O to hide · esc to cancel)"),
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

        if self.input.is_empty() && !self.busy {
            let line = Line::from(vec![
                Span::styled("> ", self.theme.prompt),
                Span::styled("Type a message… (/help for commands)", self.theme.muted),
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

        if !self.busy {
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
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let width = area.width as usize;
        let model_label = format!("{} ({})", self.model, self.thinking);
        let tokens = format_usage(self.tokens);
        let dot = " · ";
        let right = format!("{} · {}", self.status.sandbox, self.status.folder);
        let left_w = model_label.width() + dot.width() + tokens.width();
        let right_w = right.width();
        let gap = width.saturating_sub(left_w + right_w).max(2);

        let line = Line::from(vec![
            Span::styled(model_label, self.theme.thinking_model(self.thinking)),
            Span::styled(dot, self.theme.status_bar),
            Span::styled(tokens, self.theme.status_bar),
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
        "todo" => {
            let todos = v["todos"].as_array()?;
            let done = todos.iter().filter(|t| t["status"] == "completed").count();
            Some(format!("{done}/{} done", todos.len()))
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
        render_diff(diff, theme, out);
        return;
    }

    let Some(v) = parsed else {
        out.push(Line::styled(format!("  {}", summary(content)), theme.muted));
        return;
    };

    match tool {
        Some("bash") => {
            if let Some(output) = v["output"].as_str() {
                let lines: Vec<&str> = output.lines().collect();
                for line in lines.iter().take(MAX_BASH_PREVIEW_LINES) {
                    out.push(Line::styled(format!("  {line}"), theme.muted));
                }
                if lines.len() > MAX_BASH_PREVIEW_LINES {
                    let extra = lines.len() - MAX_BASH_PREVIEW_LINES;
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
            let suffix = if v["truncated"].as_bool() == Some(true) {
                " (truncated)"
            } else {
                ""
            };
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
            let total = v["total"].as_u64().unwrap_or(0);
            out.push(Line::styled(format!("  {total} matches"), theme.muted));
        }
        Some("web_fetch") => {
            let total = v["total_bytes"].as_u64().unwrap_or(0);
            let suffix = if v["truncated"].as_bool() == Some(true) {
                " (truncated)"
            } else {
                ""
            };
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
        Some("todo") => {
            for todo in v["todos"].as_array().into_iter().flatten() {
                let (mark, style) = match todo["status"].as_str() {
                    Some("completed") => ("  ", theme.todo_done),
                    Some("in_progress") => ("❯ ", Style::default()),
                    _ => ("  ", Style::default()),
                };
                let content = todo["content"].as_str().unwrap_or_default();
                out.push(Line::from(vec![
                    Span::styled(format!("  {mark}"), theme.prompt),
                    Span::styled(summary(content), style),
                ]));
            }
        }
        _ => {
            out.push(Line::styled(format!("  {}", summary(content)), theme.muted));
        }
    }
}

fn render_diff(diff: &Diff, theme: Theme, out: &mut Vec<Line<'static>>) {
    let mut emitted = 0usize;
    let mut omitted = 0usize;
    for hunk in &diff.hunks {
        if emitted < MAX_DIFF_PREVIEW_LINES {
            let header = format!(
                "  @@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
            );
            out.push(Line::styled(header, theme.diff_hunk));
            emitted += 1;
        } else {
            omitted += 1;
        }
        for line in &hunk.lines {
            if emitted >= MAX_DIFF_PREVIEW_LINES {
                omitted += 1;
                continue;
            }
            let (prefix, text, style) = match line {
                DiffLine::Context(s) => ("   ", s.as_str(), theme.muted),
                DiffLine::Removed(s) => ("  -", s.as_str(), theme.diff_remove),
                DiffLine::Added(s) => ("  +", s.as_str(), theme.diff_add),
            };
            out.push(Line::styled(format!("{prefix}{text}"), style));
            emitted += 1;
        }
    }
    if omitted > 0 || diff.truncated {
        let note = if omitted > 0 {
            format!("  … +{omitted} more diff lines")
        } else {
            "  … diff truncated".to_owned()
        };
        out.push(Line::styled(note, theme.muted));
    }
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
            out.push(Line::styled(format!("  {line}"), style));
        }
        if !self.current.is_empty() {
            out.push(Line::styled(format!("  {}", self.current), style));
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

fn format_usage(usage: TokenUsage) -> String {
    format!(
        "↑{} in ↓{} out ↻{} cached",
        ScaledTokens(usage.input),
        ScaledTokens(usage.output),
        ScaledTokens(usage.cached),
    )
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

/// First line of `text`, cut to [`SUMMARY_LEN`] characters.
pub fn summary(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(SUMMARY_LEN) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}
