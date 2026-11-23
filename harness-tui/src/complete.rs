// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Inline autocomplete popup for slash commands (`/`) and their fixed
//! arguments, and for the files a prompt names with `@`.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use harness_core::Thinking;

use crate::style::Theme;
use crate::terminal::truncate_width;

/// Maximum visible completion rows below the editor.
const MAX_COMPLETION_ROWS: u16 = 10;

/// Most files of the workspace offered after `@`.
const MAX_FILES: usize = 10_000;

/// Most files matching what follows `@` in the popup.
const MAX_FILE_ITEMS: usize = 50;

struct CommandSpec {
    name: &'static str,
    arg: Option<&'static str>,
    description: &'static str,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/model",
        arg: Some("[name]"),
        description: "pick or switch the model",
    },
    CommandSpec {
        name: "/thinking",
        arg: Some("[level]"),
        description: "set thinking effort (default, off, low ... max)",
    },
    CommandSpec {
        name: "/resume",
        arg: Some("[id]"),
        description: "pick or resume a saved session",
    },
    CommandSpec {
        name: "/output",
        arg: None,
        description: "show the full output of the last tool",
    },
    CommandSpec {
        name: "/copy",
        arg: None,
        description: "copy last assistant reply to clipboard",
    },
    CommandSpec {
        name: "/retry",
        arg: None,
        description: "send the last prompt again",
    },
    CommandSpec {
        name: "/compact",
        arg: Some("[focus]"),
        description: "summarize the conversation to save tokens",
    },
    CommandSpec {
        name: "/status",
        arg: None,
        description: "show model, session, and token usage",
    },
    CommandSpec {
        name: "/new",
        arg: None,
        description: "clear screen and start a new session",
    },
    CommandSpec {
        name: "/help",
        arg: None,
        description: "show commands and keybindings",
    },
    CommandSpec {
        name: "/quit",
        arg: None,
        description: "exit the harness",
    },
];

impl CommandSpec {
    fn label(&self) -> String {
        match self.arg {
            Some(arg) => format!("{} {arg}", self.name),
            None => self.name.to_owned(),
        }
    }

    fn insert(&self) -> String {
        match self.arg {
            Some(_) => format!("{} ", self.name),
            None => self.name.to_owned(),
        }
    }
}

const THINKING_LEVELS: &[(Thinking, &str)] = &[
    (
        Thinking::Default,
        "leave provider reasoning default untouched",
    ),
    (Thinking::Off, "disable reasoning"),
    (Thinking::Low, "light reasoning effort"),
    (Thinking::Medium, "balanced reasoning effort"),
    (Thinking::High, "deep reasoning effort"),
    (Thinking::XHigh, "extra-deep reasoning effort"),
    (Thinking::Max, "maximum reasoning effort"),
];

/// Build the multi-line `/help` text from [`COMMANDS`] so help and
/// autocomplete never drift out of sync.
pub(crate) fn help_text() -> String {
    let mut out = String::from("Commands:\n");
    for cmd in COMMANDS {
        let usage = cmd.label();
        let _ = writeln!(out, "  {usage:<19}{}", cmd.description);
    }
    let _ = writeln!(
        out,
        "  {:<19}run a shell command in the sandbox",
        "!<command>"
    );
    let _ = writeln!(out, "  {:<19}attach a file to the prompt", "@<path>");
    out.push_str(
        "\nKeys:\n\
         \x20 Enter              send prompt (queued while the agent works)\n\
         \x20 Shift/Alt+Enter    insert newline (also Ctrl-J)\n\
         \x20 Tab                autocomplete command\n\
         \x20 Shift+Tab          switch plan mode on or off\n\
         \x20 Up/Down            browse prompt history\n\
         \x20 Ctrl-O             toggle thinking visibility\n\
         \x20 Esc / Ctrl-C       cancel turn or clear input\n\
         \x20 Ctrl-D             quit",
    );
    out
}

/// One candidate shown in the autocomplete popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionItem {
    pub(crate) label: String,
    pub(crate) insert: String,
    pub(crate) description: &'static str,
}

/// Active completion state.
#[derive(Debug, Default)]
pub(crate) struct CommandCompletion {
    last_input: String,
    items: Vec<CompletionItem>,
    selected: usize,
    navigated: bool,
    dismissed: bool,
    /// The folder whose files `@` offers.
    root: Option<PathBuf>,
    /// Its files, listed the first time `@` needs them.
    files: Option<Vec<String>>,
    /// Thinking levels the model takes, when its provider says.
    thinking_levels: Option<Vec<Thinking>>,
}

impl CommandCompletion {
    pub(crate) fn is_open(&self) -> bool {
        !self.dismissed && !self.items.is_empty()
    }

    pub(crate) fn visible_rows(&self) -> u16 {
        if !self.is_open() {
            return 0;
        }
        (self.items.len() as u16).min(MAX_COMPLETION_ROWS)
    }

    pub(crate) fn navigated(&self) -> bool {
        self.navigated
    }

    pub(crate) fn selected_item(&self) -> Option<&CompletionItem> {
        self.is_open()
            .then(|| self.items.get(self.selected))
            .flatten()
    }

    pub(crate) fn dismiss(&mut self) {
        self.dismissed = true;
    }

    pub(crate) fn clear(&mut self) {
        self.last_input.clear();
        self.items.clear();
        self.selected = 0;
        self.navigated = false;
        self.dismissed = false;
    }

    /// Offer the files of `root` after `@`.
    pub(crate) fn set_root(&mut self, root: PathBuf) {
        self.root = Some(root);
        self.files = None;
    }

    pub(crate) fn thinking_levels(&self) -> Option<&[Thinking]> {
        self.thinking_levels.as_deref()
    }

    /// Offer only `levels` after `/thinking`, or every level with `None`.
    pub(crate) fn set_thinking_levels(&mut self, levels: Option<Vec<Thinking>>) {
        self.thinking_levels = levels;
    }

    /// List the files again the next time `@` needs them, as commands may
    /// have changed them.
    pub(crate) fn forget_files(&mut self) {
        self.files = None;
    }

    /// Recompute completion candidates when the editor text changes.
    pub(crate) fn update(&mut self, input: &str) {
        let mention = trailing_mention(input);
        let command = input.starts_with('/') && !input.contains('\n') && input.len() <= 64;
        if mention.is_none() && !command {
            self.clear();
            return;
        }
        if input == self.last_input {
            return;
        }
        self.last_input.clear();
        self.last_input.push_str(input);
        self.dismissed = false;
        self.navigated = false;
        self.selected = 0;
        self.items.clear();

        if let Some((start, fragment)) = mention {
            self.complete_file(&input[..start], fragment);
            return;
        }
        if let Some((cmd, rest)) = input.split_once(' ') {
            if !cmd.eq_ignore_ascii_case("/thinking") || rest.contains(char::is_whitespace) {
                return;
            }
            let prefix = rest.to_ascii_lowercase();
            let takes = |level| {
                self.thinking_levels
                    .as_ref()
                    .is_none_or(|l| l.contains(&level))
            };
            for &(level, desc) in THINKING_LEVELS {
                if takes(level) && level.as_str().starts_with(&prefix) {
                    self.items.push(CompletionItem {
                        label: format!("/thinking {level}"),
                        insert: format!("/thinking {level}"),
                        description: desc,
                    });
                }
            }
            return;
        }

        if input.contains(char::is_whitespace) {
            return;
        }

        let prefix = input.to_ascii_lowercase();
        for cmd in COMMANDS {
            if cmd.name.starts_with(&prefix) {
                self.items.push(CompletionItem {
                    label: cmd.label(),
                    insert: cmd.insert(),
                    description: cmd.description,
                });
            }
        }
    }

    /// Offer the files whose path holds `fragment`, the ones whose name
    /// starts with it first, to follow `before` and `@`.
    fn complete_file(&mut self, before: &str, fragment: &str) {
        let Some(root) = &self.root else {
            return;
        };
        let files = self.files.get_or_insert_with(|| list_files(root));
        let fragment = fragment.to_lowercase();
        let mut matches: Vec<(bool, &String)> = files
            .iter()
            .filter_map(|path| {
                let lower = path.to_lowercase();
                lower.contains(&fragment).then(|| {
                    let name = lower.rsplit('/').next().unwrap_or(&lower);
                    (!name.starts_with(&fragment), path)
                })
            })
            .collect();
        matches.sort_by_key(|&(other, path)| (other, path.len()));
        self.items = matches
            .into_iter()
            .take(MAX_FILE_ITEMS)
            .map(|(_, path)| CompletionItem {
                label: format!("@{path}"),
                insert: format!("{before}@{path} "),
                description: "",
            })
            .collect();
    }

    pub(crate) fn move_up(&mut self) -> bool {
        if !self.is_open() || self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        self.navigated = true;
        true
    }

    pub(crate) fn move_down(&mut self) -> bool {
        if !self.is_open() || self.selected + 1 >= self.items.len() {
            return false;
        }
        self.selected += 1;
        self.navigated = true;
        true
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: Theme) {
        if !self.is_open() || area.height == 0 || area.width == 0 {
            return;
        }
        let visible = area.height as usize;
        let scroll = self.selected.saturating_sub(visible.saturating_sub(1));
        let width = area.width as usize;
        let label_col = self
            .items
            .iter()
            .map(|i| i.label.width())
            .max()
            .unwrap_or(0)
            + 2;

        let mut lines = Vec::with_capacity(visible);
        for (pos, item) in self.items.iter().enumerate().skip(scroll).take(visible) {
            let selected = pos == self.selected;
            let marker = if selected { "❯ " } else { "  " };
            let marker_style = if selected { theme.prompt } else { theme.muted };
            let label_style = if selected {
                theme.user
            } else {
                theme.status_accent
            };
            let desc_style = if selected {
                theme.status_accent
            } else {
                theme.muted
            };

            let marker_w = marker.width();
            let mut spans = Vec::with_capacity(4);
            spans.push(Span::styled(marker, marker_style));

            if marker_w + label_col + 4 <= width {
                let pad = label_col.saturating_sub(item.label.width());
                spans.push(Span::styled(item.label.clone(), label_style));
                spans.push(Span::raw(" ".repeat(pad)));
                let desc_budget = width - marker_w - label_col;
                spans.push(Span::styled(
                    truncate_width(item.description, desc_budget),
                    desc_style,
                ));
            } else {
                let budget = width.saturating_sub(marker_w);
                spans.push(Span::styled(
                    truncate_width(&item.label, budget),
                    label_style,
                ));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// Where the `@` word `input` ends with starts, and what follows `@`.
fn trailing_mention(input: &str) -> Option<(usize, &str)> {
    let word = input.rsplit(char::is_whitespace).next()?;
    let start = input.len() - word.len();
    word.strip_prefix('@').map(|fragment| (start, fragment))
}

/// The files below `root`, relative to it, without the ones git or a hidden
/// name leaves out.
fn list_files(root: &Path) -> Vec<String> {
    WalkBuilder::new(root)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|entry| {
            let path = entry.path().strip_prefix(root).ok()?;
            path.to_str().map(str::to_owned)
        })
        .take(MAX_FILES)
        .collect()
}
