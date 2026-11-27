// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fmt;
use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

/// The colors to use, for the background of the terminal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
}

impl ThemeName {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

impl fmt::Display for ThemeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ThemeName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        [Self::Dark, Self::Light]
            .into_iter()
            .find(|name| name.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("invalid theme '{s}' (expected dark or light)"))
    }
}

/// The colors a theme is made of.
struct Palette {
    accent: Color,
    /// The background of a sent message.
    user_bg: Color,
    /// Rules around the editor while idle.
    faint: Color,
    muted: Color,
    tool: Color,
    tool_arg: Color,
    add: Color,
    remove: Color,
    add_bg: Color,
    remove_bg: Color,
    error: Color,
    warn: Color,
    plan: Color,
    shell: Color,
}

/// The Tokyo Night colors [`DARK`] is made of.
mod tokyo_night {
    use ratatui::style::Color;

    /// #292e42.
    pub const BG_HIGHLIGHT: Color = Color::Rgb(41, 46, 66);
    /// #565f89.
    pub const COMMENT: Color = Color::Rgb(86, 95, 137);
    /// #a9b1d6.
    pub const FG_DARK: Color = Color::Rgb(169, 177, 214);
    /// #7dcfff.
    pub const CYAN: Color = Color::Rgb(125, 207, 255);
    /// #9ece6a.
    pub const GREEN: Color = Color::Rgb(158, 206, 106);
    /// #e0af68.
    pub const YELLOW: Color = Color::Rgb(224, 175, 104);
    /// #ff9e64.
    pub const ORANGE: Color = Color::Rgb(255, 158, 100);
    /// #f7768e.
    pub const RED: Color = Color::Rgb(247, 118, 142);
    /// #bb9af7.
    pub const MAGENTA: Color = Color::Rgb(187, 154, 247);
    /// #20303b.
    pub const DIFF_ADD: Color = Color::Rgb(32, 48, 59);
    /// #37222c.
    pub const DIFF_DELETE: Color = Color::Rgb(55, 34, 44);
}

/// The colors of the Tokyo Night theme.
const DARK: Palette = {
    use tokyo_night::*;
    Palette {
        accent: CYAN,
        user_bg: BG_HIGHLIGHT,
        faint: BG_HIGHLIGHT,
        muted: COMMENT,
        tool: YELLOW,
        tool_arg: FG_DARK,
        add: GREEN,
        remove: RED,
        add_bg: DIFF_ADD,
        remove_bg: DIFF_DELETE,
        error: RED,
        warn: YELLOW,
        plan: MAGENTA,
        shell: ORANGE,
    }
};

/// Darker text and lighter backgrounds than [`DARK`], for a light
/// terminal: no color there is near white.
const LIGHT: Palette = Palette {
    accent: Color::Blue,
    user_bg: Color::Indexed(254),
    faint: Color::Indexed(250),
    muted: Color::Indexed(243),
    tool: Color::Indexed(130),
    tool_arg: Color::Reset,
    add: Color::Indexed(28),
    remove: Color::Indexed(160),
    // #d7ffd7 and #ffd7d7.
    add_bg: Color::Indexed(194),
    remove_bg: Color::Indexed(224),
    error: Color::Indexed(160),
    warn: Color::Indexed(136),
    plan: Color::Indexed(127),
    shell: Color::Indexed(161),
};

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
    /// A whole added line of a diff, tinted.
    pub add_line: Style,
    /// A whole removed line of a diff, tinted.
    pub remove_line: Style,
    pub error: Style,
    /// What needs attention soon, as a conversation near compaction.
    pub warn: Style,
    pub muted: Style,
    /// The mark of a tool call that ended well.
    pub success: Style,
    /// What joins a tool result to its call.
    pub gutter: Style,
    /// A finished `todo` item.
    pub todo_done: Style,
    /// The `todo` item in progress.
    pub todo_current: Style,
    pub status_bar: Style,
    pub status_accent: Style,
    /// Plan mode, around the editor.
    pub plan: Style,
    /// The badge of plan mode in the footer.
    pub plan_badge: Style,
    /// A shell command typed after `!`, around the editor.
    pub shell: Style,
    /// Startup logo rows, top to bottom.
    pub logo: [Style; 3],
}

impl Theme {
    /// The dark theme, plain when `color` is false (`NO_COLOR`).
    pub fn new(color: bool) -> Self {
        Self::named(ThemeName::Dark, color)
    }

    /// The theme `name`, plain when `color` is false (`NO_COLOR`).
    pub fn named(name: ThemeName, color: bool) -> Self {
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
                add_line: plain,
                remove_line: plain,
                error: plain,
                warn: plain,
                muted: plain,
                success: plain,
                gutter: plain,
                todo_done: plain,
                todo_current: plain,
                status_bar: plain,
                status_accent: plain,
                plan: plain,
                plan_badge: plain,
                shell: plain,
                logo: [plain; 3],
            };
        }
        let p = match name {
            ThemeName::Dark => &DARK,
            ThemeName::Light => &LIGHT,
        };
        let fg = |color| Style::default().fg(color);
        let bold = Modifier::BOLD;
        Self {
            color: true,
            prompt: fg(p.accent).add_modifier(bold),
            user: Style::default().bg(p.user_bg).add_modifier(bold),
            border_idle: fg(p.faint),
            border_busy: fg(p.accent),
            reasoning: fg(p.muted).add_modifier(Modifier::ITALIC),
            tool_name: fg(p.tool).add_modifier(bold),
            tool_arg: fg(p.tool_arg),
            diff_hunk: fg(p.accent),
            diff_add: fg(p.add),
            diff_remove: fg(p.remove),
            add_line: Style::default().bg(p.add_bg),
            remove_line: Style::default().bg(p.remove_bg),
            error: fg(p.error).add_modifier(bold),
            warn: fg(p.warn),
            muted: fg(p.muted),
            success: fg(p.add),
            // Not tinted inside a tinted line.
            gutter: fg(p.muted).bg(Color::Reset),
            todo_done: fg(p.muted).add_modifier(Modifier::CROSSED_OUT),
            todo_current: fg(p.tool).add_modifier(bold),
            status_bar: fg(p.muted),
            status_accent: fg(p.accent),
            plan: fg(p.plan),
            plan_badge: Style::default()
                .fg(Color::Black)
                .bg(p.plan)
                .add_modifier(bold),
            shell: fg(p.shell).add_modifier(bold),
            // Cyan to blue to violet: 256-color palette #5fd7ff, #5f87ff, #af5fff.
            logo: [81, 69, 135].map(|c| Style::default().fg(Color::Indexed(c))),
        }
    }

    /// The theme `name`, plain when the process environment says
    /// `NO_COLOR`.
    pub fn from_env(name: ThemeName) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self::named(name, !no_color)
    }
}
