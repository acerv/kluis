// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use harness_core::Error;
use ratatui::text::{Line, Span};
use tui_markdown::{BuiltinCodeTheme, CodeTheme, Options};

/// Where the colors of a code block theme come from.
enum Source {
    Builtin(BuiltinCodeTheme),
    /// A TextMate theme shipped with the binary.
    TextMate(&'static str),
}

/// The code block themes, by the names settings use.
const CODE_THEMES: [(&str, Source); 8] = [
    (
        "tokyo-night",
        Source::TextMate(include_str!("tokyo-night.tmTheme")),
    ),
    (
        "base16-ocean-dark",
        Source::Builtin(BuiltinCodeTheme::Base16OceanDark),
    ),
    (
        "base16-ocean-light",
        Source::Builtin(BuiltinCodeTheme::Base16OceanLight),
    ),
    (
        "base16-eighties-dark",
        Source::Builtin(BuiltinCodeTheme::Base16EightiesDark),
    ),
    (
        "base16-mocha-dark",
        Source::Builtin(BuiltinCodeTheme::Base16MochaDark),
    ),
    (
        "inspired-github",
        Source::Builtin(BuiltinCodeTheme::InspiredGitHub),
    ),
    (
        "solarized-dark",
        Source::Builtin(BuiltinCodeTheme::SolarizedDark),
    ),
    (
        "solarized-light",
        Source::Builtin(BuiltinCodeTheme::SolarizedLight),
    ),
];

/// The code block theme used when settings pick none.
pub fn default_code_theme(light: bool) -> Result<CodeTheme, Error> {
    code_theme(if light {
        "inspired-github"
    } else {
        "tokyo-night"
    })
}

/// The code block theme `value` names: a built-in one, or the TextMate
/// theme file at `value` when it holds a `/`.
pub fn code_theme(value: &str) -> Result<CodeTheme, Error> {
    if value.contains('/') {
        return Ok(CodeTheme::from_file(value)?);
    }
    match CODE_THEMES.iter().find(|(name, _)| *name == value) {
        Some((_, Source::Builtin(theme))) => Ok((*theme).into()),
        Some((_, Source::TextMate(source))) => Ok(CodeTheme::from_textmate(source)?),
        None => {
            let names: Vec<_> = CODE_THEMES.iter().map(|(name, _)| *name).collect();
            Err(format!(
                "code theme {value}: not one of {}, nor a file path",
                names.join(", ")
            )
            .into())
        }
    }
}

/// How Markdown is shown: styled, with syntax-highlighted code blocks, or
/// plain (`NO_COLOR`).
#[derive(Debug, Clone)]
pub struct MarkdownStyle {
    color: bool,
    table_width: u16,
    options: Options,
}

impl MarkdownStyle {
    pub fn new(color: bool, table_width: u16) -> Self {
        let mut options = Options::default().table_row_dividers(true);
        if table_width > 0 {
            options = options.table_width(table_width);
        }
        Self {
            color,
            table_width,
            options,
        }
    }

    /// Highlight code blocks with `theme`, preserving table layout options.
    pub fn set_code_theme(&mut self, theme: CodeTheme) {
        self.options = std::mem::take(&mut self.options).code_theme(theme);
    }

    /// Fit rendered tables inside `width` terminal columns. Returns `true` when
    /// the effective width changed.
    pub fn set_table_width(&mut self, width: u16) -> bool {
        if width == 0 || width == self.table_width {
            return false;
        }
        self.table_width = width;
        self.options = std::mem::take(&mut self.options).table_width(width);
        true
    }
}

/// Render a Markdown snippet into owned Ratatui lines shown with `style`.
pub fn render_markdown(input: &str, style: &MarkdownStyle) -> Vec<Line<'static>> {
    let trimmed = input.trim_matches('\n');
    if trimmed.is_empty() {
        return Vec::new();
    }
    let text = tui_markdown::from_str_with_options(trimmed, &style.options);
    text.lines
        .into_iter()
        .map(|line| into_owned_line(line, style.color))
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
/// as tokens stream in, while caching the active uncommitted lines for the
/// multi-line live preview above the spinner.
#[derive(Debug, Default)]
pub struct MarkdownStream {
    buf: String,
    committed: usize,
    parsed_prefix_end: usize,
    in_unclosed_fence: bool,
    live: Vec<Line<'static>>,
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

struct Scan {
    safe_end: usize,
    safe_state: PrefixState,
    in_unclosed_fence: bool,
}

impl MarkdownStream {
    pub fn clear(&mut self) {
        self.buf = String::new();
        self.committed = 0;
        self.parsed_prefix_end = 0;
        self.in_unclosed_fence = false;
        self.live = Vec::new();
    }

    /// Append a streaming chunk, push any newly finalized Markdown lines into
    /// `out`, and refresh the cached live preview lines.
    pub fn push_delta(&mut self, delta: &str, style: &MarkdownStyle, out: &mut Vec<Line<'static>>) {
        if delta.is_empty() {
            return;
        }
        self.buf.push_str(delta);
        let scan = scan_markdown(&self.buf);
        self.in_unclosed_fence = scan.in_unclosed_fence;

        if scan.safe_end > self.parsed_prefix_end {
            self.parsed_prefix_end = scan.safe_end;
            let prefix = &self.buf[..scan.safe_end];
            let mut lines = render_markdown(prefix, style);
            let safe_len = match scan.safe_state {
                PrefixState::InFence | PrefixState::OpenBlock => lines.len().saturating_sub(1),
                PrefixState::ClosedBlock => lines.len(),
            };
            let split = safe_len.clamp(self.committed, lines.len());
            let mut tail = lines.split_off(split);
            if split > self.committed {
                out.extend(lines.drain(self.committed..));
                self.committed = split;
            }
            if scan.safe_end == self.buf.len() {
                if matches!(scan.safe_state, PrefixState::InFence) {
                    tail.pop();
                }
                self.live = tail;
                return;
            }
        }

        let mut lines = render_markdown(&self.buf, style);
        if self.in_unclosed_fence {
            // Drop the synthetic closing fence line added by tui-markdown.
            lines.pop();
        }
        let remain = self.committed.min(lines.len());
        lines.drain(..remain);
        self.live = lines;
    }

    /// Cached uncommitted lines for the multi-line live preview above the
    /// spinner.
    pub fn live_lines(&self) -> &[Line<'static>] {
        &self.live
    }

    /// Whether any lines of the current turn have already been committed to
    /// scrollback.
    pub fn has_committed(&self) -> bool {
        self.committed > 0
    }

    /// Re-render uncommitted live lines with `style` (e.g. after a terminal
    /// resize while a table is still streaming).
    pub fn rerender_live(&mut self, style: &MarkdownStyle) {
        if self.buf.is_empty() || self.committed > 0 {
            return;
        }
        let mut lines = render_markdown(&self.buf, style);
        if self.in_unclosed_fence {
            lines.pop();
        }
        self.live = lines;
    }

    /// Flush all remaining uncommitted lines of the current turn into `out` and
    /// reset the stream. If no deltas were streamed, `fallback` is rendered.
    /// Returns `true` if any lines were emitted during the turn.
    pub fn finish(
        &mut self,
        fallback: Option<&str>,
        style: &MarkdownStyle,
        out: &mut Vec<Line<'static>>,
    ) -> bool {
        if self.buf.is_empty() {
            let Some(text) = fallback.filter(|t| !t.is_empty()) else {
                let had_output = self.committed > 0;
                self.clear();
                return had_output;
            };
            let lines = render_markdown(text, style);
            let emitted = !lines.is_empty();
            out.extend(lines);
            self.clear();
            return emitted;
        }

        let emitted = if self.in_unclosed_fence {
            let mut lines = render_markdown(&self.buf, style);
            if lines.len() > self.committed {
                out.extend(lines.drain(self.committed..));
            }
            !lines.is_empty() || self.committed > 0
        } else {
            let had_live = !self.live.is_empty();
            out.append(&mut self.live);
            had_live || self.committed > 0
        };
        self.clear();
        emitted
    }
}

fn scan_markdown(buf: &str) -> Scan {
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
                if stripped.is_some_and(|s| is_closing_fence(s, fch, flen)) {
                    in_fence = None;
                    safe_end = line_end;
                    safe_state = PrefixState::ClosedBlock;
                    pos = line_end;
                    continue;
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

    if pos < buf.len() {
        let tail = buf[pos..].strip_suffix('\r').unwrap_or(&buf[pos..]);
        let stripped = strip_up_to_3_spaces(tail);
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

    Scan {
        safe_end,
        safe_state,
        in_unclosed_fence: in_fence.is_some(),
    }
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
