// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Inline interactive filterable picker shared by `/resume` and `/model`.

use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harness_core::{ModelInfo, SessionSummary};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::style::Theme;
use crate::terminal::truncate_width;

/// Maximum visible rows inside the inline picker before scrolling.
pub(crate) const MAX_PICKER_ROWS: u16 = 8;

/// Maximum characters accepted into the live filter query.
const MAX_FILTER_CHARS: usize = 128;

/// Which domain an open [`Picker`] is selecting from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerKind {
    Session,
    Model,
}

/// Outcome of handling a key while a picker is open.
pub(crate) enum PickerOutcome {
    None,
    Redraw,
    Select(PickerKind, String),
    Cancel,
    Quit,
}

struct PickerItem {
    id: String,
    /// Display label when different from `id` (e.g. a session's first prompt).
    primary: Option<String>,
    meta: String,
    search: String,
}

impl PickerItem {
    fn primary(&self) -> &str {
        self.primary.as_deref().unwrap_or(&self.id)
    }
}

/// Interactive filterable list rendered inside the inline viewport.
pub(crate) struct Picker {
    kind: PickerKind,
    title: &'static str,
    empty_label: &'static str,
    items: Vec<PickerItem>,
    filtered: Vec<usize>,
    selected: usize,
    filter: String,
}

impl Picker {
    pub(crate) fn sessions(entries: Vec<SessionSummary>, current_id: Option<String>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let items: Vec<PickerItem> = entries
            .into_iter()
            .map(|entry| {
                let age = format_age(entry.updated_at, now);
                let msgs = if entry.message_count == 1 {
                    "1 msg".to_owned()
                } else {
                    format!("{} msgs", entry.message_count)
                };
                let badge = if current_id.as_deref() == Some(entry.id.as_str()) {
                    " (current)"
                } else {
                    ""
                };
                let meta = format!("{age} · {msgs} · {}{badge}", entry.model);
                let primary = entry.first_prompt.unwrap_or_else(|| "(empty)".to_owned());
                let search = format!("{} {} {}", entry.id, entry.model, primary).to_lowercase();
                PickerItem {
                    id: entry.id,
                    primary: Some(primary),
                    meta,
                    search,
                }
            })
            .collect();
        Self::new(
            PickerKind::Session,
            "Resume session",
            "  (no matching sessions)",
            items,
            0,
        )
    }

    pub(crate) fn models(models: &[ModelInfo], current_model: &str) -> Self {
        let mut initial_selected = 0;
        let items: Vec<PickerItem> = models
            .iter()
            .enumerate()
            .map(|(idx, model)| {
                let is_current = is_current_model(model, current_model);
                if is_current {
                    initial_selected = idx;
                }
                let badge = if is_current { " (current)" } else { "" };
                let meta = format!("{}{badge}", model.provider);
                let search = format!("{} {}", model.id, model.provider).to_lowercase();
                PickerItem {
                    id: model.id.clone(),
                    primary: None,
                    meta,
                    search,
                }
            })
            .collect();
        Self::new(
            PickerKind::Model,
            "Select model",
            "  (no matching models)",
            items,
            initial_selected,
        )
    }

    fn new(
        kind: PickerKind,
        title: &'static str,
        empty_label: &'static str,
        items: Vec<PickerItem>,
        selected: usize,
    ) -> Self {
        let filtered = (0..items.len()).collect();
        let selected = if items.is_empty() {
            0
        } else {
            selected.min(items.len() - 1)
        };
        Self {
            kind,
            title,
            empty_label,
            items,
            filtered,
            selected,
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
            (KeyCode::Enter, false) => match self.selected_item() {
                Some(item) => PickerOutcome::Select(self.kind, item.id.clone()),
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
                Span::styled(self.title, theme.prompt),
                Span::styled(
                    "  (↑/↓ move · type to filter · Enter select · Esc cancel)",
                    theme.muted,
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(self.title, theme.prompt),
                Span::styled(" · filter: ", theme.muted),
                Span::raw(self.filter.clone()),
                Span::styled(
                    format!("  ({}/{})", self.filtered.len(), self.items.len()),
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
                Paragraph::new(Line::styled(self.empty_label, theme.muted)),
                area,
            );
            return;
        }

        let visible = area.height as usize;
        let scroll = self.selected.saturating_sub(visible.saturating_sub(1));
        let width = area.width as usize;

        let mut lines = Vec::with_capacity(visible);
        for (pos, &item_idx) in self.filtered.iter().enumerate().skip(scroll).take(visible) {
            let item = &self.items[item_idx];
            lines.push(format_row(item, pos == self.selected, width, theme));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn selected_item(&self) -> Option<&PickerItem> {
        let &idx = self.filtered.get(self.selected)?;
        self.items.get(idx)
    }

    fn refilter(&mut self) {
        let query = self.filter.trim().to_lowercase();
        self.filtered.clear();
        for (idx, item) in self.items.iter().enumerate() {
            if query.is_empty() || item.search.contains(&query) {
                self.filtered.push(idx);
            }
        }
        self.selected = 0;
    }
}

fn is_current_model(model: &ModelInfo, current: &str) -> bool {
    model.id == current
        || current
            .strip_prefix(&model.provider)
            .and_then(|s| s.strip_prefix("::"))
            == Some(model.id.as_str())
}

fn format_row(item: &PickerItem, selected: bool, width: usize, theme: Theme) -> Line<'static> {
    let marker = if selected { "❯ " } else { "  " };
    let marker_w = marker.width();
    let meta_w = item.meta.width();
    let sep = " · ";
    let sep_w = sep.width();

    let mut spans = Vec::with_capacity(4);
    let marker_style = if selected { theme.prompt } else { theme.muted };
    let primary_style = if selected {
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
        let budget = width - marker_w - meta_w - sep_w;
        spans.push(Span::styled(
            truncate_width(item.primary(), budget),
            primary_style,
        ));
        spans.push(Span::styled(sep, theme.muted));
        spans.push(Span::styled(item.meta.clone(), meta_style));
    } else {
        let budget = width.saturating_sub(marker_w);
        spans.push(Span::styled(
            truncate_width(item.primary(), budget),
            primary_style,
        ));
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
