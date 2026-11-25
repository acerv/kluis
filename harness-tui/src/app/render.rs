// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Drawing the app.

use super::*;

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
            self.render_rule(frame, top_rule);
            picker.render(frame, list_area, self.theme);
            self.render_rule(frame, bottom_rule);
            self.render_footer(frame, footer_area);
            return;
        }
        let live_lines = self.wrapped_live_lines(area.width as usize);
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
            .clamp(1, MAX_LIVE_ROWS);
        let live_h = (live_lines.len() as u16).clamp(1, max_live);
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

        self.render_live(frame, live_area, live_lines);
        self.render_spinner(frame, spinner_area);
        frame.render_widget(Paragraph::new(panel_lines), panel_area);
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
        let plan = if self.plan { " · plan mode" } else { "" };
        let right = format!("{} · {}", self.status.sandbox, self.status.folder);
        // Unknown before the first request of the conversation.
        let context = match self.context {
            0 => String::new(),
            tokens => format!(" · {} context", ScaledTokens(tokens)),
        };
        let left_w = model_label.width() + plan.width() + context.width();
        let gap = width.saturating_sub(left_w + right.width()).max(2);

        let line = Line::from(vec![
            Span::styled(model_label, self.theme.thinking_model(self.thinking)),
            Span::styled(plan, self.theme.status_accent),
            Span::styled(context, self.theme.status_bar),
            Span::raw(" ".repeat(gap)),
            Span::styled(right, self.theme.status_bar),
        ]);
        frame.render_widget(Paragraph::new(line), area);
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
