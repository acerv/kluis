// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What is kept of the output of a command: the cleaned text, its end,
//! and the file holding all of it.

use std::io::Write;

use anstyle_parse::{Parser, Perform};
use tempfile::{Builder, NamedTempFile, TempPath};

use super::{MAX_BASH_LINES, MAX_BASH_LOG_BYTES, MAX_BASH_OUTPUT};

/// Removes ANSI escape sequences (colors, cursor moves, titles) and
/// control characters other than tab, newline and carriage return.
///
/// Keeps parser state between chunks, so sequences split across reads are
/// removed too.
#[derive(Default)]
pub(super) struct Filter {
    parser: Parser,
}

impl Filter {
    /// Append the cleaned `input` to `tail`, without trimming it.
    pub(super) fn clean(&mut self, input: &[u8], tail: &mut Tail) {
        for &byte in input {
            self.parser.advance(tail, byte);
        }
    }

    pub(super) fn clean_into(
        &mut self,
        input: &[u8],
        tail: &mut Tail,
        on_output: &mut impl FnMut(&str),
    ) {
        let start = tail.buf.len();
        self.clean(input, tail);
        if tail.buf.len() > start {
            if let Ok(text) = std::str::from_utf8(&tail.buf[start..]) {
                on_output(text);
            }
        }
        tail.trim_batch();
    }
}

impl Perform for Tail {
    fn print(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.buf
            .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\t' | b'\n' | b'\r') {
            self.buf.push(byte);
        }
    }
}

/// The end of the output: the last [`MAX_BASH_OUTPUT`] bytes, then the
/// last [`MAX_BASH_LINES`] lines. Overflowing bytes are streamed to a
/// temporary file so callers can inspect the full output without re-running.
#[derive(Default)]
pub(super) struct Tail {
    pub(super) buf: Vec<u8>,
    cut: bool,
    spill: Spill,
}

impl Tail {
    /// Trim in batches, so long outputs are not moved on every read.
    fn trim_batch(&mut self) {
        if self.buf.len() > 2 * MAX_BASH_OUTPUT {
            self.trim_bytes();
        }
    }

    fn trim_bytes(&mut self) {
        if self.buf.len() > MAX_BASH_OUTPUT {
            let cut = self.buf.len() - MAX_BASH_OUTPUT;
            self.spill.write(&self.buf[..cut]);
            self.buf.drain(..cut);
            self.cut = true;
        }
    }

    fn trim_lines(&mut self) {
        let last = self.buf.len().saturating_sub(1);
        if let Some(i) = memchr::memrchr_iter(b'\n', &self.buf[..last]).nth(MAX_BASH_LINES - 1) {
            self.spill.write(&self.buf[..=i]);
            self.buf.drain(..=i);
            self.cut = true;
        }
    }

    /// The kept text, whether anything was cut, and the spill file if one
    /// was saved, removed when dropped.
    pub(super) fn finish(mut self) -> (String, bool, Option<TempPath>) {
        self.trim_bytes();
        self.trim_lines();
        // Flush the kept tail before skipping leading UTF-8 continuation bytes,
        // so the spill file preserves characters split across a byte cut.
        let output_file = self.spill.finish(&self.buf);
        if self.cut {
            // Do not start in the middle of a UTF-8 character.
            let skip = self.buf.iter().take_while(|&&b| b & 0xc0 == 0x80).count();
            self.buf.drain(..skip);
        }
        let text = match String::from_utf8(self.buf) {
            Ok(text) => text,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        (text, self.cut, output_file)
    }
}

/// Lazily created file holding the full cleaned output when truncated.
///
/// Its temporary file is removed when dropped (e.g. on cancellation or
/// error), as the one [`finish`](Self::finish) returns.
#[derive(Default)]
enum Spill {
    #[default]
    Idle,
    Writing {
        file: NamedTempFile,
        remaining: u64,
    },
    /// Reached [`MAX_BASH_LOG_BYTES`]; the file is closed and kept on disk.
    Full(TempPath),
    Failed,
}

impl Spill {
    fn write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if matches!(self, Self::Idle) {
            match Builder::new()
                .prefix("harness-bash-")
                .suffix(".log")
                .tempfile()
            {
                Ok(file) => {
                    *self = Self::Writing {
                        file,
                        remaining: MAX_BASH_LOG_BYTES,
                    };
                }
                Err(_) => {
                    *self = Self::Failed;
                    return;
                }
            }
        }
        let Self::Writing { file, remaining } = self else {
            return;
        };
        let slice = &data[..data.len().min(*remaining as usize)];
        if file.write_all(slice).is_err() {
            // Transitioning to Failed drops the `NamedTempFile`, freeing `/tmp`
            // space (e.g. on ENOSPC) instead of leaving a broken partial file.
            *self = Self::Failed;
            return;
        }
        *remaining -= slice.len() as u64;
        if *remaining == 0 {
            if let Self::Writing { file, .. } = std::mem::replace(self, Self::Failed) {
                *self = Self::Full(file.into_temp_path());
            }
        }
    }

    fn finish(mut self, tail: &[u8]) -> Option<TempPath> {
        if matches!(self, Self::Writing { .. }) {
            self.write(tail);
        }
        match self {
            // Close the descriptor before exposing the path.
            Self::Writing { file, .. } => Some(file.into_temp_path()),
            Self::Full(path) => Some(path),
            Self::Idle | Self::Failed => None,
        }
    }
}
