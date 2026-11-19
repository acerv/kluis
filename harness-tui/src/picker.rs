// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Inline interactive session picker for `/resume` and `--resume`.

use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harness_core::SessionSummary;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::style::Theme;
use crate::terminal::truncate_width;

/// Maximum visible session rows inside the inline picker before scrolling.
pub(crate) const MAX_PICKER_ROWS: u16 = 8;

/// Maximum characters accepted into the live filter query.
const MAX_FILTER_CHARS: usize = 128;

/// Outcome of handling a key while the session picker is open.
pub(crate) enum PickerOutcome {
    None,
    Redraw,
    Select(String),
    Cancel,
    Quit,
}

/// Interactive filterable list of persisted sessions for the current workspace.
pub(crate) struct SessionPicker {
    entries: Vec<SessionSummary>,
    current_id: Option<String>,
    filtered: Vec<usize>,
    selected: usize,
    filter: String,
}

impl SessionPicker {
    pub(crate) fn new(entries: Vec<SessionSummary>, current_id: Option<String>) -> Self {
        let filtered = (0..entries.len()).collect();
        Self {
            entries,
            current_id,
            filtered,
            selected: 0,
            filter: String::new(),
        }
    }

    pub(crate) fn visible_rows(&self) -> u16 {
        (self.filtered.len().max(1) as u16).min(MAX_PICKER_ROWS)
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match (key.code, ctrl) {
            (KeyCode::Esc, false) | (KeyCode::Char('c'), true) => PickerOutcome::Cancel,
            (KeyCode::Char('d'), true) => PickerOutcome::Quit,
            (KeyCode::Enter, false) => match self.selected_entry() {
                Some(entry) => PickerOutcome::Select(entry.id.clone()),
                None => PickerOutcome::None,
            },
            (KeyCode::Up, false) | (KeyCode::Char('p'), true) => {
                if self.selected > 0 {
                    self.selected -= 1;
                    PickerOutcome::Redraw
                } else {
                    PickerOutcome::None
                }
            }
            (KeyCode::Down, false) | (KeyCode::Char('n'), true) => {
                if self.selected + 1 < self.filtered.len() {
                    self.selected += 1;
                    PickerOutcome::Redraw
                } else {
                    PickerOutcome::None
                }
            }
            (KeyCode::Home, false) => {
                if self.selected != 0 {
                    self.selected = 0;
                    PickerOutcome::Redraw
                } else {
                    PickerOutcome::None
                }
            }
            (KeyCode::End, false) => {
                let last = self.filtered.len().saturating_sub(1);
                if self.selected != last {
                    self.selected = last;
                    PickerOutcome::Redraw
                } else {
                    PickerOutcome::None
                }
            }
            (KeyCode::Backspace, false) => {
                if self.filter.pop().is_some() {
                    self.refilter();
                    PickerOutcome::Redraw
                } else {
                    PickerOutcome::None
                }
            }
            (KeyCode::Char(ch), false) if !alt && !ch.is_control() => {
                if self.filter.chars().count() < MAX_FILTER_CHARS {
                    self.filter.push(ch);
                    self.refilter();
                    PickerOutcome::Redraw
                } else {
                    PickerOutcome::None
                }
            }
            _ => PickerOutcome::None,
        }
    }

    pub(crate) fn paste(&mut self, text: &str) -> bool {
        let mut remaining = MAX_FILTER_CHARS.saturating_sub(self.filter.chars().count());
        let mut changed = false;
        for ch in text.chars() {
            if remaining == 0 {
                break;
            }
            let ch = if ch == '\n' || ch == '\r' { ' ' } else { ch };
            if !ch.is_control() {
                self.filter.push(ch);
                remaining -= 1;
                changed = true;
            }
        }
        if changed {
            self.refilter();
        }
        changed
    }

    pub(crate) fn header_line(&self, theme: Theme) -> Line<'static> {
        if self.filter.is_empty() {
            Line::from(vec![
                Span::styled("Resume session", theme.prompt),
                Span::styled(
                    "  (↑/↓ select · type to filter · Enter open · Esc cancel)",
                    theme.muted,
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("Resume session", theme.prompt),
                Span::styled(" · filter: ", theme.muted),
                Span::raw(self.filter.clone()),
                Span::styled(
                    format!("  ({}/{})", self.filtered.len(), self.entries.len()),
                    theme.muted,
                ),
            ])
        }
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: Theme) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        if self.filtered.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled("  (no matching sessions)", theme.muted)),
                area,
            );
            return;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let visible = area.height as usize;
        let scroll = self.selected.saturating_sub(visible.saturating_sub(1));
        let width = area.width as usize;

        let mut lines = Vec::with_capacity(visible);
        for (pos, &entry_idx) in self.filtered.iter().enumerate().skip(scroll).take(visible) {
            let entry = &self.entries[entry_idx];
            let is_selected = pos == self.selected;
            let is_current = self.current_id.as_deref() == Some(entry.id.as_str());
            lines.push(format_row(
                entry,
                is_selected,
                is_current,
                now,
                width,
                theme,
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn selected_entry(&self) -> Option<&SessionSummary> {
        let &idx = self.filtered.get(self.selected)?;
        self.entries.get(idx)
    }

    fn refilter(&mut self) {
        let query = self.filter.trim().to_lowercase();
        self.filtered.clear();
        for (idx, entry) in self.entries.iter().enumerate() {
            if query.is_empty() || matches_query(entry, &query) {
                self.filtered.push(idx);
            }
        }
        self.selected = 0;
    }
}

fn matches_query(entry: &SessionSummary, query: &str) -> bool {
    entry.id.to_lowercase().contains(query)
        || entry.model.to_lowercase().contains(query)
        || entry
            .first_prompt
            .as_deref()
            .is_some_and(|p| p.to_lowercase().contains(query))
}

fn format_row(
    entry: &SessionSummary,
    selected: bool,
    current: bool,
    now: u64,
    width: usize,
    theme: Theme,
) -> Line<'static> {
    let marker = if selected { "❯ " } else { "  " };
    let age = format_age(entry.updated_at, now);
    let msgs = if entry.message_count == 1 {
        "1 msg".to_owned()
    } else {
        format!("{} msgs", entry.message_count)
    };
    let current_badge = if current { " (current)" } else { "" };
    let meta = format!("{age} · {msgs} · {}{current_badge}", entry.model);
    let prompt = entry.first_prompt.as_deref().unwrap_or("(empty)");

    let marker_w = marker.width();
    let meta_w = meta.width();
    let sep = " · ";
    let sep_w = sep.width();

    let mut spans = Vec::with_capacity(4);
    let marker_style = if selected { theme.prompt } else { theme.muted };
    let prompt_style = if selected {
        theme.user
    } else {
        Default::default()
    };
    let meta_style = if selected {
        theme.status_accent
    } else {
        theme.muted
    };

    spans.push(Span::styled(marker, marker_style));

    if marker_w + meta_w + sep_w + 4 <= width {
        let prompt_budget = width - marker_w - meta_w - sep_w;
        spans.push(Span::styled(
            truncate_width(prompt, prompt_budget),
            prompt_style,
        ));
        spans.push(Span::styled(sep, theme.muted));
        spans.push(Span::styled(meta, meta_style));
    } else {
        let budget = width.saturating_sub(marker_w);
        spans.push(Span::styled(truncate_width(prompt, budget), prompt_style));
    }

    Line::from(spans)
}

fn format_age(timestamp: u64, now: u64) -> String {
    let elapsed = now.saturating_sub(timestamp);
    if elapsed < 60 {
        "just now".to_owned()
    } else if elapsed < 3_600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3_600)
    } else if elapsed < 86_400 * 30 {
        format!("{}d ago", elapsed / 86_400)
    } else {
        format!("{}mo ago", elapsed / (86_400 * 30))
    }
}
