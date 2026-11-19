// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

const MAX_HISTORY: usize = 200;

/// Multiline prompt editor with cursor navigation, prompt history, and
/// bracketed paste.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Input {
    buf: String,
    /// Byte offset of the cursor inside `buf` (always on a char boundary).
    cursor: usize,
    history: Vec<String>,
    /// Current index in `history` while browsing (`None` when editing live).
    history_idx: Option<usize>,
    /// Unsubmitted draft saved when entering history navigation.
    draft: String,
}

impl Input {
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Lines in the editor (always at least one, even when empty).
    pub fn lines(&self) -> Vec<&str> {
        self.buf.split('\n').collect()
    }

    /// `(row, display_col)` of the cursor (both 0-based).
    pub fn cursor_pos(&self) -> (usize, usize) {
        let before = &self.buf[..self.cursor];
        let row = before.bytes().filter(|&b| b == b'\n').count();
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let col = before[line_start..].width();
        (row, col)
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.draft.clear();
    }

    /// Take the buffer contents, record non-empty text in history, and reset.
    pub fn submit(&mut self) -> String {
        let text = std::mem::take(&mut self.buf);
        self.cursor = 0;
        self.history_idx = None;
        self.draft.clear();

        let trimmed = text.trim();
        if !trimmed.is_empty() && self.history.last().map(String::as_str) != Some(trimmed) {
            if self.history.len() >= MAX_HISTORY {
                self.history.remove(0);
            }
            self.history.push(trimmed.to_owned());
        }
        text
    }

    /// Insert pasted text at the cursor, normalizing `\r\n` and `\r` to `\n`.
    pub fn paste(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        self.buf.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.history_idx = None;
        true
    }

    /// Handle an editing key; returns true if the buffer or cursor changed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            // Newline inside the editor: Shift+Enter, Alt+Enter, or Ctrl+J.
            KeyCode::Enter if shift || alt => self.insert_char('\n'),
            KeyCode::Char('j') if ctrl => self.insert_char('\n'),

            KeyCode::Char('a') if ctrl => self.move_to(self.line_start(self.cursor)),
            KeyCode::Home if !ctrl => self.move_to(self.line_start(self.cursor)),
            KeyCode::Char('e') if ctrl => self.move_to(self.line_end(self.cursor)),
            KeyCode::End if !ctrl => self.move_to(self.line_end(self.cursor)),

            KeyCode::Char('u') if ctrl => self.delete_to_line_start(),
            KeyCode::Char('k') if ctrl => self.delete_to_line_end(),
            KeyCode::Char('w') if ctrl => self.delete_prev_word(),

            KeyCode::Left if !ctrl => self.prev_boundary().is_some_and(|p| self.move_to(p)),
            KeyCode::Right if !ctrl => self.next_boundary().is_some_and(|p| self.move_to(p)),
            KeyCode::Up if !ctrl => self.move_up_or_history(),
            KeyCode::Down if !ctrl => self.move_down_or_history(),

            KeyCode::Backspace if !ctrl => self.backspace(),
            KeyCode::Delete if !ctrl => self.delete_forward(),

            KeyCode::Char(c) if !ctrl && !alt => self.insert_char(c),
            _ => false,
        }
    }

    fn insert_char(&mut self, c: char) -> bool {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.history_idx = None;
        true
    }

    fn backspace(&mut self) -> bool {
        let Some(pos) = self.prev_boundary() else {
            return false;
        };
        self.buf.drain(pos..self.cursor);
        self.cursor = pos;
        self.history_idx = None;
        true
    }

    fn delete_forward(&mut self) -> bool {
        let Some(pos) = self.next_boundary() else {
            return false;
        };
        self.buf.drain(self.cursor..pos);
        self.history_idx = None;
        true
    }

    fn delete_to_line_start(&mut self) -> bool {
        let start = self.line_start(self.cursor);
        if start == self.cursor {
            return false;
        }
        self.buf.drain(start..self.cursor);
        self.cursor = start;
        self.history_idx = None;
        true
    }

    fn delete_to_line_end(&mut self) -> bool {
        let end = self.line_end(self.cursor);
        if end == self.cursor {
            return false;
        }
        self.buf.drain(self.cursor..end);
        self.history_idx = None;
        true
    }

    fn delete_prev_word(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let mut pos = self.cursor;
        // Skip trailing whitespace before the word (staying on the same line
        // unless cursor is right after `\n`).
        while let Some((i, c)) = self.buf[..pos].char_indices().next_back() {
            if c == '\n' {
                if pos == self.cursor {
                    pos = i;
                }
                break;
            }
            if !c.is_whitespace() {
                break;
            }
            pos = i;
        }
        while let Some((i, c)) = self.buf[..pos].char_indices().next_back() {
            if c.is_whitespace() {
                break;
            }
            pos = i;
        }
        if pos == self.cursor {
            return false;
        }
        self.buf.drain(pos..self.cursor);
        self.cursor = pos;
        self.history_idx = None;
        true
    }

    fn move_up_or_history(&mut self) -> bool {
        let start = self.line_start(self.cursor);
        if start > 0 {
            // Move to the previous line within the multiline input.
            let col_chars = self.buf[start..self.cursor].chars().count();
            let prev_end = start - 1;
            let prev_start = self.line_start(prev_end);
            let target = advance_chars(&self.buf[prev_start..prev_end], col_chars);
            return self.move_to(prev_start + target);
        }
        self.history_prev()
    }

    fn move_down_or_history(&mut self) -> bool {
        let end = self.line_end(self.cursor);
        if end < self.buf.len() {
            // Move to the next line within the multiline input.
            let start = self.line_start(self.cursor);
            let col_chars = self.buf[start..self.cursor].chars().count();
            let next_start = end + 1;
            let next_end = self.line_end(next_start);
            let target = advance_chars(&self.buf[next_start..next_end], col_chars);
            return self.move_to(next_start + target);
        }
        self.history_next()
    }

    fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next_idx = match self.history_idx {
            None => {
                self.draft = self.buf.clone();
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next_idx);
        self.buf = self.history[next_idx].clone();
        self.cursor = self.buf.len();
        true
    }

    fn history_next(&mut self) -> bool {
        let Some(idx) = self.history_idx else {
            return false;
        };
        if idx + 1 < self.history.len() {
            let next = idx + 1;
            self.history_idx = Some(next);
            self.buf = self.history[next].clone();
        } else {
            self.history_idx = None;
            self.buf = std::mem::take(&mut self.draft);
        }
        self.cursor = self.buf.len();
        true
    }

    fn move_to(&mut self, pos: usize) -> bool {
        if self.cursor == pos {
            return false;
        }
        self.cursor = pos;
        true
    }

    fn line_start(&self, pos: usize) -> usize {
        self.buf[..pos].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.buf[pos..]
            .find('\n')
            .map_or(self.buf.len(), |i| pos + i)
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.buf[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }
}

fn advance_chars(line: &str, count: usize) -> usize {
    line.char_indices()
        .nth(count)
        .map_or(line.len(), |(i, _)| i)
}
