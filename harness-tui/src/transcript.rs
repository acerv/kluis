// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What was printed above the viewport, kept to print it again.

use std::collections::VecDeque;

use ratatui::text::Line;

use crate::app::ToolRun;

/// Most entries kept to print again when the terminal width or the
/// output mode changes.
const MAX_ENTRIES: usize = 10_000;

/// Most bytes of tool results kept whole: past them, the oldest results
/// keep only their short form.
const MAX_TOOL_BYTES: usize = 4 * 1024 * 1024;

/// A piece of the conversation as the scrollback shows it, rendered when
/// printed, so that it can follow the output mode.
#[derive(Debug, Clone)]
pub enum Entry {
    Line(Line<'static>),
    /// A row of thinking, shown in verbose mode only.
    Reasoning(Line<'static>),
    /// A tool call and its result, short or whole by the output mode.
    Tool(Box<ToolRun>),
    /// A tool call past [`MAX_TOOL_BYTES`], kept in its short form.
    Frozen(Vec<Line<'static>>),
}

/// The last entries printed, with a bound on their count and on the bytes
/// of the tool results they hold.
#[derive(Debug, Default)]
pub struct Transcript {
    entries: VecDeque<Entry>,
    /// Bytes of the tool results in `entries`.
    tool_bytes: usize,
}

impl Transcript {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.tool_bytes = 0;
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Keep `entries`, dropping the oldest ones past [`MAX_ENTRIES`], and
    /// turning the oldest tool calls into their short form, as `freeze`
    /// renders it, past [`MAX_TOOL_BYTES`].
    pub fn push(
        &mut self,
        entries: impl IntoIterator<Item = Entry>,
        freeze: impl Fn(&ToolRun) -> Vec<Line<'static>>,
    ) {
        for entry in entries {
            self.tool_bytes += tool_bytes(&entry);
            self.entries.push_back(entry);
        }
        let extra = self.entries.len().saturating_sub(MAX_ENTRIES);
        for entry in self.entries.drain(..extra) {
            self.tool_bytes -= tool_bytes(&entry);
        }
        let mut i = 0;
        while self.tool_bytes > MAX_TOOL_BYTES && i < self.entries.len() {
            if let Entry::Tool(run) = &self.entries[i] {
                self.tool_bytes -= run.size();
                self.entries[i] = Entry::Frozen(freeze(run));
            }
            i += 1;
        }
    }
}

fn tool_bytes(entry: &Entry) -> usize {
    match entry {
        Entry::Tool(run) => run.size(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bytes: usize) -> Entry {
        Entry::Tool(Box::new(ToolRun::finished("bash", "x".repeat(bytes))))
    }

    fn frozen(t: &Transcript) -> usize {
        t.iter().filter(|e| matches!(e, Entry::Frozen(_))).count()
    }

    #[test]
    fn keeps_the_last_entries() {
        let mut t = Transcript::default();
        let lines = (0..MAX_ENTRIES + 5).map(|i| Entry::Line(Line::raw(i.to_string())));
        t.push(lines, |_| Vec::new());
        assert_eq!(t.entries.len(), MAX_ENTRIES);
        let Some(Entry::Line(first)) = t.iter().next() else {
            panic!("not a line");
        };
        assert_eq!(first.to_string(), "5");
    }

    #[test]
    fn freezes_the_oldest_results_past_the_budget() {
        let mut t = Transcript::default();
        // A little less than half, with the arguments of the call.
        let half = MAX_TOOL_BYTES / 2 - 16;
        t.push([run(half), run(half)], |_| vec![Line::raw("short")]);
        assert_eq!(frozen(&t), 0);
        t.push([run(half)], |_| vec![Line::raw("short")]);
        assert_eq!(frozen(&t), 1);
        assert!(matches!(t.iter().next(), Some(Entry::Frozen(_))));
        assert!(t.tool_bytes <= MAX_TOOL_BYTES);
    }

    #[test]
    fn counts_the_bytes_of_dropped_entries() {
        let mut t = Transcript::default();
        t.push([run(1000)], |_| Vec::new());
        let lines = (0..MAX_ENTRIES).map(|_| Entry::Line(Line::raw("")));
        t.push(lines, |_| Vec::new());
        assert_eq!(t.tool_bytes, 0);
    }
}
