// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use harness_core::Thinking;
use ratatui::style::{Color, Modifier, Style};

/// Styles used across the TUI. All styles are plain when `NO_COLOR` is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub color: bool,
    pub prompt: Style,
    pub user: Style,
    pub border_idle: Style,
    pub border_busy: Style,
    pub reasoning: Style,
    pub tool_name: Style,
    pub tool_arg: Style,
    pub diff_hunk: Style,
    pub diff_add: Style,
    pub diff_remove: Style,
    pub error: Style,
    pub muted: Style,
    /// A finished `todo` item.
    pub todo_done: Style,
    /// The `todo` item in progress.
    pub todo_current: Style,
    pub status_bar: Style,
    pub status_accent: Style,
    /// Startup logo rows, top to bottom.
    pub logo: [Style; 3],
}

impl Theme {
    /// Build a theme, honoring `NO_COLOR` when `color` is false.
    pub fn new(color: bool) -> Self {
        if !color {
            let plain = Style::default();
            return Self {
                color: false,
                prompt: plain,
                user: plain,
                border_idle: plain,
                border_busy: plain,
                reasoning: plain,
                tool_name: plain,
                tool_arg: plain,
                diff_hunk: plain,
                diff_add: plain,
                diff_remove: plain,
                error: plain,
                muted: plain,
                todo_done: plain,
                todo_current: plain,
                status_bar: plain,
                status_accent: plain,
                logo: [plain; 3],
            };
        }
        Self {
            color: true,
            prompt: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            user: Style::default()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
            border_idle: Style::default().fg(Color::DarkGray),
            border_busy: Style::default().fg(Color::Cyan),
            reasoning: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            tool_name: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            tool_arg: Style::default().fg(Color::Gray),
            diff_hunk: Style::default().fg(Color::Cyan),
            diff_add: Style::default().fg(Color::Green),
            diff_remove: Style::default().fg(Color::Red),
            error: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            muted: Style::default().fg(Color::DarkGray),
            todo_done: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::CROSSED_OUT),
            todo_current: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            status_bar: Style::default().fg(Color::DarkGray),
            status_accent: Style::default().fg(Color::Cyan),
            // Cyan to blue to violet: 256-color palette #5fd7ff, #5f87ff, #af5fff.
            logo: [81, 69, 135].map(|c| Style::default().fg(Color::Indexed(c))),
        }
    }

    /// Build a theme from the process environment (`NO_COLOR`).
    pub fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self::new(!no_color)
    }

    /// Footer model color keyed by thinking effort.
    pub fn thinking_model(&self, thinking: Thinking) -> Style {
        if !self.color {
            return Style::default();
        }
        let fg = match thinking {
            Thinking::Default => Color::Green,
            Thinking::Off => Color::Gray,
            Thinking::Low => Color::Cyan,
            Thinking::Medium => Color::Yellow,
            Thinking::High => Color::Indexed(208),
            Thinking::XHigh => Color::Red,
            Thinking::Max => Color::Magenta,
        };
        Style::default().fg(fg)
    }
}
