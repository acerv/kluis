// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fmt;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, DiffOp, DiffTag, TextDiff};

use crate::edit::Match;

/// Unchanged context lines included before and after each change.
pub const DIFF_CONTEXT_LINES: usize = 3;

/// Maximum bytes of line content kept in a [`Diff`]; larger diffs stop at
/// the last line that fits and set [`Diff::truncated`].
pub const MAX_DIFF_BYTES: usize = 256 * 1024;

/// A structured diff between the old and new versions of an edited file.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub hunks: Vec<DiffHunk>,
    /// 1-indexed line number of the first change in the new file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
    /// True when the diff hit [`MAX_DIFF_BYTES`] and later lines or hunks
    /// were omitted.
    pub truncated: bool,
}

/// One contiguous region of changes and surrounding context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// 1-indexed first line in the old file (`0` when `old_lines == 0` at start).
    pub old_start: usize,
    /// Number of lines from the old file in this hunk.
    pub old_lines: usize,
    /// 1-indexed first line in the new file (`0` when `new_lines == 0` at start).
    pub new_start: usize,
    /// Number of lines from the new file in this hunk.
    pub new_lines: usize,
    pub lines: Vec<DiffLine>,
}

/// One line inside a [`DiffHunk`], without its trailing newline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLine {
    Context(String),
    Removed(String),
    Added(String),
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for hunk in &self.hunks {
            write!(f, "{hunk}")?;
        }
        Ok(())
    }
}

impl fmt::Display for DiffHunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "@@ -{} +{} @@",
            RangeFmt(self.old_start, self.old_lines),
            RangeFmt(self.new_start, self.new_lines),
        )?;
        for line in &self.lines {
            match line {
                DiffLine::Context(s) => writeln!(f, " {s}")?,
                DiffLine::Removed(s) => writeln!(f, "-{s}")?,
                DiffLine::Added(s) => writeln!(f, "+{s}")?,
            }
        }
        Ok(())
    }
}

struct RangeFmt(usize, usize);

impl fmt::Display for RangeFmt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.1 == 1 {
            write!(f, "{}", self.0)
        } else {
            write!(f, "{},{}", self.0, self.1)
        }
    }
}

struct Window {
    start: usize,
    end: usize,
    matches: Range<usize>,
}

/// Build a [`Diff`] from the LF-normalized original text and sorted,
/// non-overlapping `matches`, diffing only merged line windows around edits.
pub(crate) fn from_matches(lf: &str, matches: &[Match<'_>]) -> Diff {
    let mut builder = DiffBuilder::default();
    let mut prev_byte = 0;
    let mut old_line_offset = 0;
    let mut new_line_offset = 0;

    for win in merge_windows(lf, matches) {
        let skipped = lf[prev_byte..win.start]
            .bytes()
            .filter(|&b| b == b'\n')
            .count();
        old_line_offset += skipped;
        new_line_offset += skipped;

        let old_slice = &lf[win.start..win.end];
        let new_window = apply_window(lf, &win, matches);
        let text_diff = line_diff(old_slice, &new_window);

        for group in text_diff.grouped_ops(DIFF_CONTEXT_LINES) {
            builder.push_group(&text_diff, &group, old_line_offset, new_line_offset);
            if builder.diff.truncated {
                return builder.diff;
            }
        }

        if let Some(last) = text_diff.ops().last() {
            old_line_offset += last.old_range().end;
            new_line_offset += last.new_range().end;
        }
        prev_byte = win.end;
    }

    builder.diff
}

/// Diff `new` against `old` line by line, up to [`MAX_DIFF_BYTES`].
pub fn diff_lines(old: &str, new: &str) -> Diff {
    let text_diff = line_diff(old, new);
    let mut builder = DiffBuilder::default();
    for group in text_diff.grouped_ops(DIFF_CONTEXT_LINES) {
        builder.push_group(&text_diff, &group, 0, 0);
        if builder.diff.truncated {
            break;
        }
    }
    builder.diff
}

/// How many lines `new` adds to `old` and removes from it.
pub fn count_lines(old: &str, new: &str) -> (usize, usize) {
    line_diff(old, new)
        .ops()
        .iter()
        .fold((0, 0), |(added, removed), op| {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            match tag {
                DiffTag::Equal => (added, removed),
                _ => (added + new_range.len(), removed + old_range.len()),
            }
        })
}

fn line_diff<'a>(old: &'a str, new: &'a str) -> TextDiff<'a, 'a, str> {
    TextDiff::configure()
        .algorithm(Algorithm::Histogram)
        .diff_lines(old, new)
}

#[derive(Default)]
struct DiffBuilder {
    diff: Diff,
    used_bytes: usize,
}

impl DiffBuilder {
    fn push_group(
        &mut self,
        text_diff: &TextDiff<'_, '_, str>,
        group: &[DiffOp],
        old_line_offset: usize,
        new_line_offset: usize,
    ) {
        let Some(first) = group.first() else {
            return;
        };
        let old_idx = old_line_offset + first.old_range().start;
        let new_idx = new_line_offset + first.new_range().start;

        if self.diff.first_changed_line.is_none() {
            if let Some(op) = group.iter().find(|op| !matches!(op, DiffOp::Equal { .. })) {
                self.diff.first_changed_line = Some(new_line_offset + op.new_range().start + 1);
            }
        }

        let mut lines = Vec::new();
        let mut old_lines = 0;
        let mut new_lines = 0;
        let mut has_change = false;

        for change in group.iter().flat_map(|op| text_diff.iter_changes(op)) {
            let text = change.value().strip_suffix('\n').unwrap_or(change.value());
            // Charge 1 byte per line so empty lines also count toward the cap.
            let cost = text.len() + 1;
            if self.used_bytes + cost > MAX_DIFF_BYTES {
                self.diff.truncated = true;
                break;
            }
            self.used_bytes += cost;
            match change.tag() {
                ChangeTag::Equal => {
                    old_lines += 1;
                    new_lines += 1;
                    lines.push(DiffLine::Context(text.to_owned()));
                }
                ChangeTag::Delete => {
                    old_lines += 1;
                    has_change = true;
                    lines.push(DiffLine::Removed(text.to_owned()));
                }
                ChangeTag::Insert => {
                    new_lines += 1;
                    has_change = true;
                    lines.push(DiffLine::Added(text.to_owned()));
                }
            }
        }

        // Skip hunks truncated before any changed line could be included.
        if has_change {
            let old_start = if old_lines == 0 { old_idx } else { old_idx + 1 };
            let new_start = if new_lines == 0 { new_idx } else { new_idx + 1 };
            self.diff.hunks.push(DiffHunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines,
            });
        }
    }
}

/// Expand each match to whole lines plus surrounding context, merging
/// overlapping or touching windows.
fn merge_windows(lf: &str, matches: &[Match<'_>]) -> Vec<Window> {
    let mut windows: Vec<Window> = Vec::new();
    for (i, m) in matches.iter().enumerate() {
        let win_start = step_back_lines(lf, line_start(lf, m.start), DIFF_CONTEXT_LINES);
        let win_end = step_forward_lines(lf, line_end(lf, m.end), DIFF_CONTEXT_LINES);
        if let Some(prev) = windows.last_mut().filter(|w| w.end >= win_start) {
            prev.end = prev.end.max(win_end);
            prev.matches.end = i + 1;
        } else {
            windows.push(Window {
                start: win_start,
                end: win_end,
                matches: i..i + 1,
            });
        }
    }
    windows
}

fn apply_window(lf: &str, win: &Window, matches: &[Match<'_>]) -> String {
    let mut out = String::with_capacity(win.end - win.start);
    let mut pos = win.start;
    for m in &matches[win.matches.clone()] {
        out.push_str(&lf[pos..m.start]);
        out.push_str(m.new_text);
        pos = m.end;
    }
    out.push_str(&lf[pos..win.end]);
    out
}

/// Start byte of the line containing `pos`.
fn line_start(s: &str, pos: usize) -> usize {
    s[..pos].rfind('\n').map_or(0, |i| i + 1)
}

/// End byte (past `\n`, if present) of the line containing `pos`.
fn line_end(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    s[pos..].find('\n').map_or(s.len(), |i| pos + i + 1)
}

/// Move a line-start offset back by up to `count` lines.
fn step_back_lines(s: &str, mut start: usize, count: usize) -> usize {
    for _ in 0..count {
        if start == 0 {
            break;
        }
        start = line_start(s, start - 1);
    }
    start
}

/// Move a line-end offset forward by up to `count` lines.
fn step_forward_lines(s: &str, mut end: usize, count: usize) -> usize {
    for _ in 0..count {
        if end >= s.len() {
            break;
        }
        end = line_end(s, end);
    }
    end
}
