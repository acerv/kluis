// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Editable prompt input buffer.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Input {
    buf: String,
    /// Byte offset of the cursor inside `buf` (always on a char boundary).
    cursor: usize,
}

impl Input {
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.buf
    }

    /// Cursor column in characters from the start of the buffer.
    pub fn cursor_chars(&self) -> usize {
        self.buf[..self.cursor].chars().count()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    /// Handle an editing key; returns true if the buffer or cursor changed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('a'), true) | (KeyCode::Home, false) => self.move_to(0),
            (KeyCode::Char('e'), true) | (KeyCode::End, false) => self.move_to(self.buf.len()),
            (KeyCode::Char('u'), true) => {
                if self.cursor == 0 {
                    return false;
                }
                self.buf.drain(..self.cursor);
                self.cursor = 0;
                true
            }
            (KeyCode::Char('k'), true) => {
                if self.cursor == self.buf.len() {
                    return false;
                }
                self.buf.truncate(self.cursor);
                true
            }
            (KeyCode::Left, false) => match self.prev_boundary() {
                Some(pos) => self.move_to(pos),
                None => false,
            },
            (KeyCode::Right, false) => match self.next_boundary() {
                Some(pos) => self.move_to(pos),
                None => false,
            },
            (KeyCode::Backspace, false) => match self.prev_boundary() {
                Some(pos) => {
                    self.buf.drain(pos..self.cursor);
                    self.cursor = pos;
                    true
                }
                None => false,
            },
            (KeyCode::Delete, false) => match self.next_boundary() {
                Some(pos) => {
                    self.buf.drain(self.cursor..pos);
                    true
                }
                None => false,
            },
            (KeyCode::Char(c), false) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.buf.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                true
            }
            _ => false,
        }
    }

    fn move_to(&mut self, pos: usize) -> bool {
        if self.cursor == pos {
            return false;
        }
        self.cursor = pos;
        true
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
