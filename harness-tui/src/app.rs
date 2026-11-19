// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use harness_core::{Event as AgentEvent, ToolDetails};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::input::Input;
use crate::style::Theme;
use crate::Status;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Maximum characters shown in a single-line tool argument or result summary.
const SUMMARY_LEN: usize = 120;

/// Maximum bytes retained in each live streaming preview buffer.
const MAX_LIVE_PREVIEW: usize = 4 * 1024;

/// Maximum lines shown for the live streaming preview inside the inline viewport.
const MAX_LIVE_LINES: usize = 3;

/// Fixed height of the inline viewport (live activity + input + status bar).
pub const VIEWPORT_HEIGHT: u16 = (MAX_LIVE_LINES as u16) + 2;

/// Owned agent progress update passed over the channel from [`Agent::prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentUpdate {
    TurnStart,
    ReasoningDelta(String),
    TextDelta(String),
    Text(String),
    TurnEnd {
        input_tokens: u64,
        output_tokens: u64,
    },
    ToolCall {
        name: String,
        args: String,
    },
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
            AgentEvent::TurnEnd(usage) => {
                let usage = usage?;
                Self::TurnEnd {
                    input_tokens: usage.prompt_tokens.unwrap_or(0).max(0) as u64,
                    output_tokens: usage.completion_tokens.unwrap_or(0).max(0) as u64,
                }
            }
            AgentEvent::ToolCall(call) => Self::ToolCall {
                name: call.fn_name.clone(),
                args: call.fn_arguments.to_string(),
            },
            AgentEvent::ToolResult(resp, details) => Self::ToolResult {
                content: resp.content.clone(),
                details: details.cloned(),
            },
        })
    }
}

/// High-level action produced by handling a terminal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Submit(String),
    Cancel,
    Quit,
}

/// Pure TUI state machine, decoupled from terminal I/O so it can be tested
/// directly with ratatui's `TestBackend`.
pub struct App {
    model: String,
    status: Status,
    theme: Theme,
    input: Input,
    busy: bool,
    spinner: usize,
    active_tool: Option<String>,
    streaming_text: String,
    streaming_reasoning: String,
    input_tokens: u64,
    output_tokens: u64,
    notice: Option<String>,
    scrollback: Vec<Line<'static>>,
}

impl App {
    pub fn new(model: impl Into<String>, status: Status, theme: Theme) -> Self {
        Self {
            model: model.into(),
            status,
            theme,
            input: Input::default(),
            busy: false,
            spinner: 0,
            active_tool: None,
            streaming_text: String::new(),
            streaming_reasoning: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            notice: None,
            scrollback: Vec::new(),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    pub fn theme(&self) -> Theme {
        self.theme
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

    /// Record a submitted user prompt and enter the busy state.
    pub fn start_prompt(&mut self, text: &str) {
        self.busy = true;
        self.spinner = 0;
        self.active_tool = None;
        self.streaming_text.clear();
        self.streaming_reasoning.clear();
        self.notice = None;
        self.scrollback.push(Line::from(vec![
            Span::styled("> ", self.theme.prompt),
            Span::styled(text.to_owned(), self.theme.user),
        ]));
    }

    /// Apply a streaming update from the running agent turn.
    pub fn apply_update(&mut self, update: AgentUpdate) {
        match update {
            AgentUpdate::TurnStart => {
                self.active_tool = None;
                self.streaming_text.clear();
                self.streaming_reasoning.clear();
            }
            AgentUpdate::ReasoningDelta(delta) => {
                push_tail(&mut self.streaming_reasoning, &delta, MAX_LIVE_PREVIEW);
            }
            AgentUpdate::TextDelta(delta) => {
                push_tail(&mut self.streaming_text, &delta, MAX_LIVE_PREVIEW);
            }
            AgentUpdate::Text(text) => {
                self.streaming_text.clear();
                self.streaming_reasoning.clear();
                for line in text.lines() {
                    self.scrollback.push(Line::raw(line.to_owned()));
                }
                self.scrollback.push(Line::raw(String::new()));
            }
            AgentUpdate::TurnEnd {
                input_tokens,
                output_tokens,
            } => {
                self.input_tokens = self.input_tokens.saturating_add(input_tokens);
                self.output_tokens = self.output_tokens.saturating_add(output_tokens);
            }
            AgentUpdate::ToolCall { name, args } => {
                self.active_tool = Some(name.clone());
                self.scrollback.push(Line::styled(
                    format!("→ {name} {}", summary(&args)),
                    self.theme.tool,
                ));
            }
            AgentUpdate::ToolResult { content, details } => {
                self.active_tool = None;
                self.scrollback.push(Line::styled(
                    format!("← {}", summary(&content)),
                    self.theme.muted,
                ));
                if let Some(ToolDetails::Edit(diff)) = details {
                    for line in diff.to_string().lines() {
                        self.scrollback
                            .push(Line::styled(line.to_owned(), self.theme.muted));
                    }
                }
            }
        }
    }

    /// Leave the busy state when the prompt finishes or fails.
    pub fn finish_prompt(&mut self, outcome: Result<(), String>) {
        self.busy = false;
        self.active_tool = None;
        self.streaming_text.clear();
        self.streaming_reasoning.clear();
        if let Err(err) = outcome {
            self.scrollback
                .push(Line::styled(format!("error: {err}"), self.theme.error));
        }
    }

    /// Leave the busy state after the user cancels an in-flight prompt.
    pub fn cancel_prompt(&mut self) {
        self.busy = false;
        self.active_tool = None;
        self.streaming_text.clear();
        self.streaming_reasoning.clear();
        self.scrollback
            .push(Line::styled("[cancelled]", self.theme.muted));
    }

    /// Process a crossterm terminal event and return the next action.
    pub fn handle_event(&mut self, event: TermEvent) -> Action {
        match event {
            TermEvent::Resize(_, _) => Action::Redraw,
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            _ => Action::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => {
                if self.busy {
                    return Action::Cancel;
                }
                if !self.input.is_empty() {
                    self.input.clear();
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

        self.notice = None;
        if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            let text = self.input.take();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Action::Redraw;
            }
            return Action::Submit(trimmed.to_owned());
        }

        if self.input.handle_key(key) {
            Action::Redraw
        } else {
            Action::None
        }
    }

    /// Draw the inline viewport (live activity, input line, status bar).
    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let [live_area, input_area, status_area] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        self.render_live(frame, live_area);
        self.render_input(frame, input_area);
        self.render_status(frame, status_area);
    }

    fn render_live(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let mut lines = Vec::new();
        if self.busy {
            let spin = SPINNER[self.spinner];
            let label = match &self.active_tool {
                Some(tool) => format!("{spin} Running {tool}… (Esc/Ctrl-C to cancel)"),
                None => format!("{spin} Thinking… (Esc/Ctrl-C to cancel)"),
            };
            lines.push(Line::styled(label, self.theme.muted));

            let preview = if !self.streaming_text.is_empty() {
                Some((&self.streaming_text, ratatui::style::Style::default()))
            } else if !self.streaming_reasoning.is_empty() {
                Some((&self.streaming_reasoning, self.theme.reasoning))
            } else {
                None
            };
            if let Some((text, style)) = preview {
                let tail_cap = area.height.saturating_sub(1) as usize;
                let preview_lines: Vec<&str> = text.lines().collect();
                let start = preview_lines.len().saturating_sub(tail_cap);
                for l in &preview_lines[start..] {
                    lines.push(Line::styled((*l).to_owned(), style));
                }
            }
        } else if let Some(notice) = &self.notice {
            lines.push(Line::styled(notice.clone(), self.theme.muted));
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_input(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let prompt = Span::styled("> ", self.theme.prompt);
        let text = Span::raw(self.input.text());
        frame.render_widget(Paragraph::new(Line::from(vec![prompt, text])), area);

        if !self.busy {
            let x = area
                .x
                .saturating_add(2)
                .saturating_add(self.input.cursor_chars() as u16)
                .min(area.right().saturating_sub(1));
            frame.set_cursor_position(Position::new(x, area.y));
        }
    }

    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let tokens = format_tokens(self.input_tokens, self.output_tokens);
        let spans = vec![
            Span::styled(self.model.as_str(), self.theme.status_accent),
            Span::styled(" · ", self.theme.status_bar),
            Span::styled(self.status.sandbox.as_str(), self.theme.status_bar),
            Span::styled(" · ", self.theme.status_bar),
            Span::styled(tokens, self.theme.status_bar),
            Span::styled(" · ", self.theme.status_bar),
            Span::styled(self.status.folder.as_str(), self.theme.status_bar),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn push_tail(buf: &mut String, delta: &str, cap: usize) {
    buf.push_str(delta);
    if buf.len() > cap {
        let cut = buf.len() - cap;
        let boundary = buf.ceil_char_boundary(cut);
        buf.drain(..boundary);
    }
}

fn format_tokens(input: u64, output: u64) -> String {
    format!("↑{input} ↓{output}")
}

/// First line of `text`, cut to [`SUMMARY_LEN`] characters.
pub fn summary(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(SUMMARY_LEN) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}
