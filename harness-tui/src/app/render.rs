// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Drawing the app.

use super::*;

/// Marks the line the user types on.
const PROMPT: &str = "❯ ";

impl App {
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
            + self.panel_lines().len() as u16
            + self.editor_rows(width)
            + self.completion.visible_rows()
    }

    /// The plan, then the queued messages, between the spinner and the
    /// editor.
    fn panel_lines(&self) -> Vec<Line<'static>> {
        let mut lines = self.todo_lines();
        lines.extend(self.queued_lines());
        lines
    }

    /// The messages waiting to join the conversation, in order, the
    /// latest ones folded into how many more there are.
    fn queued_lines(&self) -> Vec<Line<'static>> {
        if self.queued.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![Line::from(vec![
            Span::styled("Queued", self.theme.prompt),
            Span::styled(format!(" {}", self.queued.len()), self.theme.muted),
        ])];
        for text in self.queued.iter().take(MAX_QUEUED_ROWS) {
            lines.push(Line::from(vec![
                Span::styled("  › ", self.theme.muted),
                Span::raw(summary(text)),
            ]));
        }
        let hidden = self.queued.len().saturating_sub(MAX_QUEUED_ROWS);
        if hidden > 0 {
            lines.push(Line::styled(
                format!("  … +{hidden} more"),
                self.theme.muted,
            ));
        }
        lines
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
        let (head, body) = self.wrapped_live_lines(width as usize);
        live_height(&head, &body)
    }

    fn editor_rows(&self, width: u16) -> u16 {
        let text_w = (width as usize).saturating_sub(2).max(1);
        let layout = layout_editor(&self.input, text_w);
        (layout.rows.len() as u16).clamp(1, MAX_EDITOR_ROWS)
    }

    /// What shows above the spinner, wrapped to `width`: the lines that
    /// stay, the running tool call, then those whose tail shows.
    fn wrapped_live_lines(&self, width: usize) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
        let width = width.max(1);
        if !self.markdown.live_lines().is_empty() {
            (Vec::new(), wrap_lines(self.markdown.live_lines(), width))
        } else if let Some(tool) = &self.active_tool {
            let mut call = Vec::new();
            format_running_tool(tool, self.verbose, self.theme, &mut call);
            let output = self.tool_preview.render_lines(self.theme.muted);
            (wrap_lines(&call, width), wrap_lines(&output, width))
        } else if self.verbose && !self.live_reasoning.is_empty() {
            let line = Line::styled(self.live_reasoning.clone(), self.theme.reasoning);
            (Vec::new(), wrap_lines(&[line], width))
        } else {
            (Vec::new(), Vec::new())
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
        self.render_in(frame, frame.area());
    }

    /// Draw the `/diff` panel in `area`.
    pub fn render_diff_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        self.diff.render(frame, area, self.theme);
    }

    /// Draw the viewport in `area` of the frame.
    pub fn render_in(&self, frame: &mut Frame<'_>, area: Rect) {
        self.cursor.set(None);
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
            self.render_rule(frame, top_rule, None);
            picker.render(frame, list_area, self.theme);
            self.render_rule(frame, bottom_rule, None);
            self.render_footer(frame, footer_area);
            return;
        }
        let (live_head, live_body) = self.wrapped_live_lines(area.width as usize);
        let panel_lines = self.panel_lines();
        let completion_h = self
            .completion
            .visible_rows()
            .min(area.height.saturating_sub(FIXED_CHROME_ROWS + 2));
        let panel_h = (panel_lines.len() as u16).min(
            area.height
                .saturating_sub(FIXED_CHROME_ROWS + 2 + completion_h),
        );
        let max_live = area
            .height
            .saturating_sub(FIXED_CHROME_ROWS + completion_h + panel_h + 1)
            .max(1);
        let live_h = live_height(&live_head, &live_body).min(max_live);
        let editor_h = area
            .height
            .saturating_sub(FIXED_CHROME_ROWS + live_h + panel_h + completion_h)
            .max(1);
        let [live_area, spinner_area, panel_area, top_rule, editor_area, bottom_rule, completion_area, footer_area] =
            Layout::vertical([
                Constraint::Length(live_h),
                Constraint::Length(1),
                Constraint::Length(panel_h),
                Constraint::Length(1),
                Constraint::Length(editor_h),
                Constraint::Length(1),
                Constraint::Length(completion_h),
                Constraint::Fill(1),
            ])
            .areas(area);

        render_live(frame, live_area, live_head, live_body);
        self.render_spinner(frame, spinner_area);
        frame.render_widget(Paragraph::new(panel_lines), panel_area);
        let mode = self.editor_mode();
        self.render_rule(frame, top_rule, mode);
        self.render_editor(frame, editor_area, mode);
        self.render_rule(frame, bottom_rule, mode.map(|(_, style)| ("", style)));
        self.completion.render(frame, completion_area, self.theme);
        self.render_footer(frame, footer_area);
    }

    fn render_spinner(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let line = if self.busy {
            let mut spans = vec![Span::styled(
                format!("{} ", SPINNER[self.spinner]),
                self.theme.status_accent,
            )];
            spans.push(Span::raw(if self.active_tool.is_some() {
                "Running"
            } else if self.is_thinking {
                "Thinking"
            } else {
                "Working"
            }));
            let elapsed = self
                .activity_started
                .map(|t| format_elapsed(t.elapsed().as_secs()))
                .filter(|s| !s.is_empty())
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            let tokens = match self.prompt_output {
                0 => String::new(),
                n => format!(" · ↓ {} tokens", ScaledTokens(n)),
            };
            spans.push(Span::styled(
                format!("…{elapsed}{tokens}"),
                self.theme.muted,
            ));
            Line::from(spans)
        } else if let Some(notice) = &self.notice {
            Line::styled(notice.as_str(), self.theme.muted)
        } else {
            return;
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    /// What the editor is in, if anything special, with its color: a
    /// shell command, a slash command, or plan mode.
    fn editor_mode(&self) -> Option<(&'static str, Style)> {
        let text = self.input.text();
        if text.starts_with('!') {
            Some(("shell", self.theme.shell))
        } else if text.starts_with('/') {
            Some(("command", self.theme.status_accent))
        } else if self.plan {
            Some(("plan", self.theme.plan))
        } else {
            None
        }
    }

    /// A rule across `area`, in the color of `mode` if any, its name in the
    /// rule, else in the color telling whether the agent works.
    fn render_rule(&self, frame: &mut Frame<'_>, area: Rect, mode: Option<(&str, Style)>) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let (label, style) = match mode {
            Some((label, style)) => (label, style),
            None if self.busy => ("", self.theme.border_busy),
            None => ("", self.theme.border_idle),
        };
        let width = area.width as usize;
        let line = if label.is_empty() {
            Line::styled("─".repeat(width), style)
        } else {
            let label = format!(" {label} ");
            let rest = width.saturating_sub(2 + label.width());
            Line::from(vec![
                Span::styled("──", style),
                Span::styled(label, style),
                Span::styled("─".repeat(rest), style),
            ])
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_editor(&self, frame: &mut Frame<'_>, area: Rect, mode: Option<(&str, Style)>) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let visible_rows = area.height as usize;
        let prefix_w = PROMPT.width();
        let prompt = match mode {
            Some((_, style)) => style,
            None => self.theme.prompt,
        };
        let text_w = (area.width as usize).saturating_sub(prefix_w).max(1);

        if self.input.is_empty() {
            let hint = if self.busy {
                "Type a message to send while the agent works…"
            } else if self.plan {
                "Describe what to plan… (Shift+Tab to leave plan mode)"
            } else {
                "Type a message… (/help for commands)"
            };
            let line = Line::from(vec![
                Span::styled(PROMPT, prompt),
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
                Span::styled(PROMPT, prompt)
            } else {
                Span::raw("  ")
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
        let theme = self.theme;
        let mut left = vec![Span::raw(format!("{} ", self.model))];
        left.extend(effort_spans(self.thinking, theme));
        if self.plan {
            let badge = if theme.color { " plan " } else { "[plan]" };
            left.push(Span::raw(" "));
            left.push(Span::styled(badge, theme.plan_badge));
        }
        if self.verbose {
            left.push(Span::styled(" · verbose", theme.status_bar));
        }
        let (text, near) = match self.compact_at {
            Some(limit) => (
                format!(
                    " · {}/{} context",
                    ScaledTokens(self.context),
                    ScaledTokens(limit)
                ),
                self.context.saturating_mul(100) >= limit.saturating_mul(NEAR_COMPACT_PERCENT),
            ),
            None => (format!(" · {} context", ScaledTokens(self.context)), false),
        };
        let style = if near { theme.warn } else { theme.status_bar };
        left.push(Span::styled(text, style));
        let right = if self.busy {
            let mode = if self.verbose { "quiet" } else { "verbose" };
            format!("esc to cancel · ctrl+o {mode} · {}", self.status.sandbox)
        } else {
            format!("{} · {}", self.status.sandbox, self.status.folder)
        };
        let left_w: usize = left.iter().map(Span::width).sum();
        let gap = (area.width as usize)
            .saturating_sub(left_w + right.width())
            .max(2);
        left.push(Span::raw(" ".repeat(gap)));
        left.push(Span::styled(right, theme.status_bar));
        frame.render_widget(Paragraph::new(Line::from(left)), area);
    }
}

/// Cells of the thinking effort gauge: one per level above off.
const EFFORT_CELLS: usize = Thinking::ALL.len() - 2;

/// `thinking` as a gauge filled by effort, then its name; only the name
/// for the default of the provider, whose effort is unknown.
fn effort_spans(thinking: Thinking, theme: Theme) -> Vec<Span<'static>> {
    let name = Span::styled(thinking.as_str(), theme.status_bar);
    let level = Thinking::ALL.iter().position(|&t| t == thinking);
    let Some(filled) = level.and_then(|level| level.checked_sub(1)) else {
        return vec![name];
    };
    vec![
        Span::styled("▰".repeat(filled), theme.status_accent),
        Span::styled("▱".repeat(EFFORT_CELLS - filled), theme.border_idle),
        Span::raw(" "),
        name,
    ]
}

/// Rows the live lines take: all of `head`, and up to [`MAX_LIVE_ROWS`]
/// of `body`, at least one.
fn live_height(head: &[Line<'static>], body: &[Line<'static>]) -> u16 {
    let rows = head.len() + body.len().min(MAX_LIVE_ROWS as usize);
    u16::try_from(rows).unwrap_or(u16::MAX).max(1)
}

/// Draw `head` at the top of `area`, then as much of the tail of `body` as
/// fits below it.
fn render_live(
    frame: &mut Frame<'_>,
    area: Rect,
    mut head: Vec<Line<'static>>,
    mut body: Vec<Line<'static>>,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let visible = area.height as usize;
    head.truncate(visible);
    let room = visible - head.len();
    if body.len() > room {
        body.drain(..body.len() - room);
    }
    head.append(&mut body);
    frame.render_widget(Paragraph::new(head), area);
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
