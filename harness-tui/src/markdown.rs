// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use ratatui::text::{Line, Span};

/// Render a Markdown snippet into owned Ratatui lines with syntax-highlighted
/// code blocks, or plain unstyled text when `color` is false (`NO_COLOR`).
pub fn render_markdown(input: &str, color: bool) -> Vec<Line<'static>> {
    let trimmed = input.trim_matches('\n');
    if trimmed.is_empty() {
        return Vec::new();
    }
    let text = tui_markdown::from_str(trimmed);
    text.lines
        .into_iter()
        .map(|line| into_owned_line(line, color))
        .collect()
}

fn into_owned_line(line: Line<'_>, color: bool) -> Line<'static> {
    if !color {
        let plain: String = line.spans.into_iter().map(|s| s.content).collect();
        return Line::raw(plain);
    }
    let style = line.style;
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    Line::from(spans).style(style)
}

/// Incremental Markdown renderer that commits finalized lines into scrollback
/// as tokens stream in, while exposing the active incomplete line for live
/// preview above the spinner.
#[derive(Debug, Default)]
pub struct MarkdownStream {
    buf: String,
    committed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefixState {
    /// Inside an unclosed fenced code block (`tui-markdown` appends a synthetic
    /// closing fence line at the end, so hold back 1 line).
    InFence,
    /// Ends at a closed block boundary (blank line, closing code fence, or ATX
    /// heading), so all rendered lines are final.
    ClosedBlock,
    /// Ends inside an open paragraph, list, or blockquote (hold back the last
    /// line in case the next line continues it via soft break or Setext
    /// underline).
    OpenBlock,
}

impl MarkdownStream {
    pub fn clear(&mut self) {
        self.buf.clear();
        self.committed = 0;
    }

    /// Append a streaming chunk and push any newly finalized Markdown lines
    /// into `out`.
    pub fn push_delta(&mut self, delta: &str, color: bool, out: &mut Vec<Line<'static>>) {
        self.buf.push_str(delta);
        let Some((prefix, state)) = committable_prefix(&self.buf) else {
            return;
        };
        let mut lines = render_markdown(prefix, color);
        let safe_len = match state {
            PrefixState::InFence | PrefixState::OpenBlock => lines.len().saturating_sub(1),
            PrefixState::ClosedBlock => lines.len(),
        };
        if safe_len > self.committed {
            out.extend(lines.drain(self.committed..safe_len));
            self.committed = safe_len;
        }
    }

    /// The currently active (uncommitted) line for the 1-row live preview above
    /// the spinner, if any.
    pub fn live_line(&self, color: bool) -> Option<Line<'static>> {
        if self.buf.is_empty() {
            return None;
        }
        let mut lines = render_markdown(&self.buf, color);
        if is_in_unclosed_fence(&self.buf) {
            // Drop the synthetic closing fence line added by tui-markdown.
            lines.pop();
        }
        if lines.len() <= self.committed {
            return None;
        }
        lines.pop()
    }

    /// Flush all remaining uncommitted lines of the current turn into `out` and
    /// reset the stream. If no deltas were streamed, `fallback` is rendered.
    /// Returns `true` if any lines were emitted during the turn.
    pub fn finish(
        &mut self,
        fallback: Option<&str>,
        color: bool,
        out: &mut Vec<Line<'static>>,
    ) -> bool {
        if self.buf.is_empty() {
            if let Some(text) = fallback.filter(|t| !t.is_empty()) {
                self.buf.push_str(text);
            } else {
                let had_output = self.committed > 0;
                self.clear();
                return had_output;
            }
        }
        let mut lines = render_markdown(&self.buf, color);
        if lines.len() > self.committed {
            out.extend(lines.drain(self.committed..));
        }
        let emitted = !lines.is_empty() || self.committed > 0;
        self.clear();
        emitted
    }
}

fn committable_prefix(buf: &str) -> Option<(&str, PrefixState)> {
    let mut in_fence: Option<(char, usize)> = None;
    let mut safe_end = 0usize;
    let mut safe_state = PrefixState::ClosedBlock;
    let mut pos = 0usize;

    while let Some(rel) = buf[pos..].find('\n') {
        let line_end = pos + rel + 1;
        let line = buf[pos..line_end - 1]
            .strip_suffix('\r')
            .unwrap_or(&buf[pos..line_end - 1]);
        let stripped = strip_up_to_3_spaces(line);

        match in_fence {
            Some((fch, flen)) => {
                if let Some(stripped) = stripped {
                    if is_closing_fence(stripped, fch, flen) {
                        in_fence = None;
                        safe_end = line_end;
                        safe_state = PrefixState::ClosedBlock;
                        pos = line_end;
                        continue;
                    }
                }
                safe_end = line_end;
                safe_state = PrefixState::InFence;
            }
            None => {
                if let Some(fence) = stripped.and_then(parse_opening_fence) {
                    in_fence = Some(fence);
                    safe_end = line_end;
                    safe_state = PrefixState::InFence;
                } else if line.trim().is_empty() || stripped.is_some_and(is_atx_heading) {
                    safe_end = line_end;
                    safe_state = PrefixState::ClosedBlock;
                } else if line.contains('|') {
                    // Hold GFM table rows until a blank or non-table line closes
                    // the table so column widths do not shift after committing.
                } else {
                    safe_end = line_end;
                    safe_state = PrefixState::OpenBlock;
                }
            }
        }
        pos = line_end;
    }

    (safe_end > 0).then_some((&buf[..safe_end], safe_state))
}

fn is_in_unclosed_fence(buf: &str) -> bool {
    let mut in_fence: Option<(char, usize)> = None;
    for raw in buf.lines() {
        let stripped = strip_up_to_3_spaces(raw);
        match in_fence {
            Some((fch, flen)) => {
                if stripped.is_some_and(|s| is_closing_fence(s, fch, flen)) {
                    in_fence = None;
                }
            }
            None => {
                if let Some(fence) = stripped.and_then(parse_opening_fence) {
                    in_fence = Some(fence);
                }
            }
        }
    }
    in_fence.is_some()
}

/// CommonMark block markers may be indented by at most 3 leading spaces.
fn strip_up_to_3_spaces(line: &str) -> Option<&str> {
    let spaces = line.bytes().take_while(|&b| b == b' ').count();
    if spaces > 3 {
        return None;
    }
    Some(&line[spaces..])
}

fn is_atx_heading(stripped: &str) -> bool {
    let hashes = stripped.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes)
        && stripped
            .as_bytes()
            .get(hashes)
            .is_none_or(|&b| b == b' ' || b == b'\t')
}

fn parse_opening_fence(stripped: &str) -> Option<(char, usize)> {
    let fch = stripped.chars().next().filter(|&c| c == '`' || c == '~')?;
    let count = stripped.chars().take_while(|&c| c == fch).count();
    if count < 3 {
        return None;
    }
    let rest = &stripped[count..];
    // CommonMark: backtick info strings cannot contain backticks.
    if fch == '`' && rest.contains('`') {
        return None;
    }
    Some((fch, count))
}

fn is_closing_fence(stripped: &str, fch: char, min_len: usize) -> bool {
    let count = stripped.chars().take_while(|&c| c == fch).count();
    count >= min_len && stripped[count..].trim().is_empty()
}
