// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use ratatui::style::{Color, Modifier, Style};

/// Styles used across the TUI. All styles are plain when `NO_COLOR` is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub color: bool,
    pub prompt: Style,
    pub user: Style,
    pub reasoning: Style,
    pub tool: Style,
    pub error: Style,
    pub muted: Style,
    pub status_bar: Style,
    pub status_accent: Style,
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
                reasoning: plain,
                tool: plain,
                error: plain,
                muted: plain,
                status_bar: plain,
                status_accent: plain,
            };
        }
        Self {
            color: true,
            prompt: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            user: Style::default().add_modifier(Modifier::BOLD),
            reasoning: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            tool: Style::default().fg(Color::Yellow),
            error: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            muted: Style::default().fg(Color::DarkGray),
            status_bar: Style::default().fg(Color::DarkGray),
            status_accent: Style::default().fg(Color::Cyan),
        }
    }

    /// Build a theme from the process environment (`NO_COLOR`).
    pub fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self::new(!no_color)
    }
}
