// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use harness_core::tools::{Diff, DiffLine};
use harness_core::{Event as AgentEvent, ToolDetails};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::input::Input;
use crate::markdown::MarkdownStream;
use crate::style::Theme;
use crate::Status;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Maximum characters shown in a single-line summary.
const SUMMARY_LEN: usize = 120;

/// Maximum bytes retained in an unterminated live streaming line.
const MAX_LIVE_LINE_BYTES: usize = 4 * 1024;

/// Maximum diff lines shown inline in scrollback per `edit` call.
const MAX_DIFF_PREVIEW_LINES: usize = 24;

/// Maximum output lines shown inline in scrollback per `bash` call.
const MAX_BASH_PREVIEW_LINES: usize = 8;

/// Maximum visible rows inside the framed multiline editor before scrolling.
const MAX_EDITOR_ROWS: u16 = 8;

/// Non-editor chrome rows in the inline viewport:
/// 1 live streaming line + 1 status/spinner line + top rule + bottom rule +
/// 2 footer lines.
const CHROME_ROWS: u16 = 6;

/// Initial height of the inline viewport (1-row editor + chrome).
pub const VIEWPORT_HEIGHT: u16 = CHROME_ROWS + 1;

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

/// Interactive slash commands handled locally by the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Model(Option<String>),
    Clear,
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
    show_thinking: bool,
    spinner: usize,
    active_tool: Option<String>,
    /// Incremental Markdown stream for the current assistant turn.
    markdown: MarkdownStream,
    /// Current incomplete streaming reasoning line.
    live_reasoning: String,
    /// Whether any reasoning lines were emitted during the current turn.
    emitted_reasoning: bool,
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
            show_thinking: true,
            spinner: 0,
            active_tool: None,
            markdown: MarkdownStream::default(),
            live_reasoning: String::new(),
            emitted_reasoning: false,
            input_tokens: 0,
            output_tokens: 0,
            notice: None,
            scrollback: Vec::new(),
        }
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

    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Push an informational muted line into scrollback (e.g. slash output).
    pub fn push_info(&mut self, msg: impl Into<String>) {
        self.scrollback
            .push(Line::styled(msg.into(), self.theme.muted));
    }

    /// Reset displayed token counters (e.g. after `/clear`).
    pub fn reset_tokens(&mut self) {
        self.input_tokens = 0;
        self.output_tokens = 0;
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
        self.clear_live();
        self.notice = None;
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
                self.clear_live();
            }
            AgentUpdate::ReasoningDelta(delta) => {
                if !self.show_thinking {
                    return;
                }
                self.emitted_reasoning = true;
                stream_lines(
                    &mut self.live_reasoning,
                    &delta,
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
            AgentUpdate::TurnEnd {
                input_tokens,
                output_tokens,
            } => {
                self.input_tokens = self.input_tokens.saturating_add(input_tokens);
                self.output_tokens = self.output_tokens.saturating_add(output_tokens);
            }
            AgentUpdate::ToolCall { name, args } => {
                self.flush_reasoning();
                self.flush_text();
                self.active_tool = Some(name.clone());
                self.scrollback
                    .push(format_tool_call(&name, &args, self.theme));
            }
            AgentUpdate::ToolResult { content, details } => {
                let tool_name = self.active_tool.take();
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
        self.active_tool = None;
        self.clear_live();
        self.scrollback
            .push(Line::styled("[cancelled]", self.theme.muted));
        self.scrollback.push(Line::raw(String::new()));
    }

    /// Process a crossterm terminal event and return the next action.
    pub fn handle_event(&mut self, event: TermEvent) -> Action {
        match event {
            TermEvent::Resize(_, _) => Action::Redraw,
            TermEvent::Paste(text) if !self.busy => {
                self.notice = None;
                if self.input.paste(&text) {
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('t'), true) => {
                self.show_thinking = !self.show_thinking;
                if !self.show_thinking {
                    self.live_reasoning.clear();
                }
                self.notice = Some(if self.show_thinking {
                    "Thinking visible (Ctrl-T to hide)".to_owned()
                } else {
                    "Thinking hidden (Ctrl-T to show)".to_owned()
                });
                return Action::Redraw;
            }
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
            Action::Redraw
        } else {
            Action::None
        }
    }

    fn clear_live(&mut self) {
        self.markdown.clear();
        self.live_reasoning.clear();
        self.emitted_reasoning = false;
    }

    fn flush_reasoning(&mut self) {
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

    /// Desired inline viewport height for `width`, growing with editor lines
    /// and soft-wrapped text up to [`MAX_EDITOR_ROWS`].
    pub fn viewport_height(&self, width: u16) -> u16 {
        CHROME_ROWS + self.editor_rows(width)
    }

    fn editor_rows(&self, width: u16) -> u16 {
        let text_w = (width as usize).saturating_sub(2).max(1);
        let layout = layout_editor(&self.input, text_w);
        (layout.rows.len() as u16).clamp(1, MAX_EDITOR_ROWS)
    }

    /// Draw the pi-style inline viewport: streaming line, spinner/notice,
    /// framed multiline editor with cursor, and 2-line footer.
    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let editor_h = area.height.saturating_sub(CHROME_ROWS).max(1);
        let [live_area, spinner_area, top_rule, editor_area, bottom_rule, footer_area] =
            Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(editor_h),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(area);

        self.render_live_line(frame, live_area);
        self.render_spinner(frame, spinner_area);
        self.render_rule(frame, top_rule);
        self.render_editor(frame, editor_area);
        self.render_rule(frame, bottom_rule);
        self.render_footer(frame, footer_area);
    }

    fn render_live_line(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let width = area.width as usize;
        let line = if let Some(md_line) = self.markdown.live_line(self.theme.color) {
            if md_line.width() <= width {
                md_line
            } else {
                Line::raw(tail_to_width(&md_line.to_string(), width))
            }
        } else if self.show_thinking && !self.live_reasoning.is_empty() {
            Line::styled(
                tail_to_width(&self.live_reasoning, width),
                self.theme.reasoning,
            )
        } else {
            return;
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_spinner(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let line = if self.busy {
            let spin = SPINNER[self.spinner];
            let label = match &self.active_tool {
                Some(tool) => format!("{spin} Running {tool}…  (esc to cancel)"),
                None if !self.show_thinking => {
                    format!("{spin} Thinking…  (Ctrl-T to show · esc to cancel)")
                }
                None => format!("{spin} Working…  (esc to cancel)"),
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
            frame.set_cursor_position(Position::new(
                area.x.saturating_add(prefix_w as u16),
                area.y,
            ));
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
            frame.set_cursor_position(Position::new(x, y));
        }
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let width = area.width as usize;

        let left_top = match &self.status.git_branch {
            Some(branch) => format!("{} ({branch})", self.status.folder),
            None => self.status.folder.clone(),
        };
        let row1 = split_footer_line(
            &left_top,
            self.theme.status_bar,
            &self.status.sandbox,
            self.theme.status_bar,
            width,
        );

        let tokens = format!("↑{} ↓{}", self.input_tokens, self.output_tokens);
        let row2 = Line::from(vec![
            Span::styled(self.model.clone(), self.theme.status_accent),
            Span::styled("  ", self.theme.status_bar),
            Span::styled(tokens, self.theme.status_bar),
        ]);

        let lines = if area.height >= 2 {
            vec![row1, row2]
        } else {
            vec![row2]
        };
        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// Append `delta` to `live`, flushing every completed `\n`-terminated line
/// straight into `scrollback` with `style`.
fn stream_lines(live: &mut String, delta: &str, style: Style, scrollback: &mut Vec<Line<'static>>) {
    live.push_str(delta);
    while let Some(nl) = live.find('\n') {
        let line = live[..nl].strip_suffix('\r').unwrap_or(&live[..nl]);
        scrollback.push(Line::styled(line.to_owned(), style));
        live.drain(..=nl);
    }
    if live.len() > MAX_LIVE_LINE_BYTES {
        let cut = live.len() - MAX_LIVE_LINE_BYTES;
        let boundary = live.ceil_char_boundary(cut);
        live.drain(..boundary);
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
    Some(match cmd {
        "/model" => Ok(SlashCommand::Model(arg)),
        "/clear" => Ok(SlashCommand::Clear),
        "/quit" | "/exit" => Ok(SlashCommand::Quit),
        "/help" => Ok(SlashCommand::Help),
        other => Err(other.to_owned()),
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

fn split_footer_line(
    left: &str,
    left_style: Style,
    right: &str,
    right_style: Style,
    width: usize,
) -> Line<'static> {
    let left_w = Span::raw(left).width();
    let right_w = Span::raw(right).width();
    let gap = width.saturating_sub(left_w + right_w);
    if gap >= 2 {
        Line::from(vec![
            Span::styled(left.to_owned(), left_style),
            Span::raw(" ".repeat(gap)),
            Span::styled(right.to_owned(), right_style),
        ])
    } else {
        Line::from(vec![
            Span::styled(left.to_owned(), left_style),
            Span::raw("  "),
            Span::styled(right.to_owned(), right_style),
        ])
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

fn char_width(ch: char) -> usize {
    let mut tmp = [0u8; 4];
    Span::raw(&*ch.encode_utf8(&mut tmp)).width()
}

fn tail_to_width(line: &str, width: usize) -> String {
    if Span::raw(line).width() <= width {
        return line.to_owned();
    }
    let mut used = 0usize;
    let mut start = line.len();
    for (idx, ch) in line.char_indices().rev() {
        let w = char_width(ch);
        if used + w > width {
            break;
        }
        used += w;
        start = idx;
    }
    line[start..].to_owned()
}

/// First line of `text`, cut to [`SUMMARY_LEN`] characters.
pub fn summary(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(SUMMARY_LEN) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}
