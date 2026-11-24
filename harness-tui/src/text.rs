// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Measuring, cutting and wrapping text for the terminal.

use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of a single Unicode character in terminal cells.
pub(crate) fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// Truncate `text` to fit within `max_width` terminal cells, appending `…` if cut.
pub(crate) fn truncate_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.width() <= max_width {
        return text.to_owned();
    }
    let ellipsis = '…';
    let ellipsis_w = char_width(ellipsis);
    let budget = max_width.saturating_sub(ellipsis_w);
    let mut out = String::new();
    let mut col = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if col + w > budget {
            break;
        }
        out.push(ch);
        col += w;
    }
    if ellipsis_w <= max_width {
        out.push(ellipsis);
    }
    out
}

/// Count how many rows `lines` occupy when soft-wrapped to `width` cells,
/// without allocating wrapped lines.
pub(crate) fn wrapped_line_count(lines: &[Line<'static>], width: usize) -> usize {
    let width = width.max(1);
    let mut total = 0usize;
    for line in lines {
        if line.width() <= width {
            total += 1;
            continue;
        }
        let mut col = 0usize;
        let mut rows = 1usize;
        for span in &line.spans {
            for ch in span.content.chars() {
                let ch_w = char_width(ch);
                if col > 0 && col + ch_w > width {
                    rows += 1;
                    col = 0;
                }
                col += ch_w;
            }
        }
        total += rows;
    }
    total
}

/// Wrap styled lines so each resulting line fits within `width` cells.
pub(crate) fn wrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= width {
            out.push(line.clone());
            continue;
        }
        let line_style = line.style;
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize;
        for span in &line.spans {
            let style = line_style.patch(span.style);
            let mut chunk = String::new();
            for ch in span.content.chars() {
                let ch_w = char_width(ch);
                if col > 0 && col + ch_w > width {
                    if !chunk.is_empty() {
                        current.push(Span::styled(std::mem::take(&mut chunk), style));
                    }
                    out.push(Line::from(std::mem::take(&mut current)));
                    col = 0;
                }
                chunk.push(ch);
                col += ch_w;
            }
            if !chunk.is_empty() {
                current.push(Span::styled(chunk, style));
            }
        }
        if !current.is_empty() {
            out.push(Line::from(current));
        }
    }
    out
}
