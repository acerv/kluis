// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! How tool calls and their results show.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::Duration;

use harness_core::tools::{
    Diff, DiffLine, BASH_TOOL, EDIT_TOOL, FIND_TOOL, GREP_TOOL, LS_TOOL, READ_TOOL, WRITE_TOOL,
};
use harness_core::ToolDetails;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::{summary, MAX_LIVE_ROWS};
use crate::style::Theme;
use crate::text::char_width;

/// Maximum diff lines shown inline in scrollback per `edit` call.
const MAX_DIFF_PREVIEW_LINES: usize = 24;

/// Maximum output lines shown inline in scrollback per `bash` call.
const MAX_BASH_PREVIEW_LINES: usize = 8;

/// Maximum bytes retained per live tool output preview line.
const MAX_TOOL_PREVIEW_LINE_BYTES: usize = 1024;

/// Shortest run of a tool that shows how long it took.
const MIN_SHOWN_ELAPSED: Duration = Duration::from_secs(1);

/// What joins the first line of a result to its call, and the indent of
/// the others.
const RESULT_JOINT: &str = "  ⎿ ";
const RESULT_INDENT: &str = "    ";

/// The name of a tool and what it works on, in one line.
pub(super) fn format_tool_call(name: &str, args: &str, theme: Theme) -> Line<'static> {
    let parsed: Option<Value> = serde_json::from_str(args).ok();
    let detail = parsed.as_ref().and_then(|v| match name {
        READ_TOOL => {
            let path = v["path"].as_str()?;
            match v["offset"].as_u64() {
                Some(offset) => Some(format!("{path}:{offset}")),
                None => Some(path.to_owned()),
            }
        }
        WRITE_TOOL | EDIT_TOOL => v["path"].as_str().map(str::to_owned),
        BASH_TOOL => v["command"].as_str().map(summary),
        GREP_TOOL | FIND_TOOL => {
            let pat = v["pattern"].as_str()?;
            match v["path"].as_str() {
                Some(path) if !path.is_empty() => Some(format!("{pat} {path}")),
                _ => Some(pat.to_owned()),
            }
        }
        LS_TOOL => Some(v["path"].as_str().unwrap_or(".").to_owned()),
        "ask_user" => v["question"].as_str().map(summary),
        "web_fetch" => {
            let url = v["url"].as_str()?;
            match v["offset"].as_u64() {
                Some(offset) => Some(format!("{url} @{offset}")),
                None => Some(url.to_owned()),
            }
        }
        "web_search" => v["query"].as_str().map(summary),
        "skill" => v["name"].as_str().map(str::to_owned),
        "task" => v["prompt"].as_str().map(summary),
        _ => None,
    });
    let detail = detail.unwrap_or_else(|| summary(args));
    Line::from(vec![
        Span::styled(name.to_owned(), theme.tool_name),
        Span::raw(" "),
        Span::styled(detail, theme.tool_arg),
    ])
}

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunStatus {
    Done,
    Failed,
    /// It never ended: the prompt was cancelled or failed meanwhile.
    Interrupted,
}

/// `run` as the scrollback shows it: its call and how it ended, then the
/// short form of its result, or the whole of it when `verbose`.
pub(super) fn format_tool_run(
    run: &ToolRun,
    verbose: bool,
    theme: Theme,
    out: &mut Vec<Line<'static>>,
) {
    let parsed: Option<Value> = run
        .reply
        .as_ref()
        .and_then(|reply| serde_json::from_str(&reply.content).ok());
    let status = match (&run.reply, &parsed) {
        (None, _) => RunStatus::Interrupted,
        (Some(_), Some(v)) if failed(v) => RunStatus::Failed,
        _ => RunStatus::Done,
    };
    out.push(run_header(run, status, theme));
    let Some(reply) = &run.reply else {
        out.push(Line::default());
        return;
    };
    let start = out.len();
    if verbose {
        format_full_reply(reply, parsed.as_ref(), theme, out);
    } else {
        let tool = run.call.as_ref().map(|c| c.name.as_str());
        format_reply(tool, reply, parsed.as_ref(), theme, out);
    }
    for (i, line) in out[start..].iter_mut().enumerate() {
        let joint = if i == 0 { RESULT_JOINT } else { RESULT_INDENT };
        line.spans.insert(0, Span::styled(joint, theme.gutter));
    }
    out.push(Line::default());
}

/// Whether the result `v` of a tool says it failed.
fn failed(v: &Value) -> bool {
    v["error"].is_string()
        || v["exit_code"].as_i64().is_some_and(|code| code != 0)
        || v["timed_out"].as_bool() == Some(true)
}

/// The mark of how `run` ended, its call, and how long it took.
fn run_header(run: &ToolRun, status: RunStatus, theme: Theme) -> Line<'static> {
    let (mark, style) = match status {
        RunStatus::Done => ("✓", theme.success),
        RunStatus::Failed => ("✗", theme.error),
        RunStatus::Interrupted => ("✗", theme.muted),
    };
    let mut spans = vec![Span::styled(mark, style), Span::raw(" ")];
    match &run.call {
        Some(call) => spans.extend(format_tool_call(&call.name, &call.args, theme).spans),
        // A restored result whose call is missing.
        None => spans.push(Span::styled("tool", theme.tool_name)),
    }
    if status == RunStatus::Interrupted {
        spans.push(Span::styled(" · interrupted", theme.muted));
    } else if let Some(elapsed) = run.elapsed.filter(|e| *e >= MIN_SHOWN_ELAPSED) {
        let elapsed = format_elapsed(elapsed.as_secs());
        spans.push(Span::styled(format!(" · {elapsed}"), theme.muted));
    }
    Line::from(spans)
}

/// The short form of the result of `tool`.
fn format_reply(
    tool: Option<&str>,
    reply: &ToolReply,
    parsed: Option<&Value>,
    theme: Theme,
    out: &mut Vec<Line<'static>>,
) {
    if let Some(err) = parsed.and_then(|v| v["error"].as_str()) {
        out.push(Line::styled(format!("error: {err}"), theme.error));
        return;
    }
    if let Some(diff) = &reply.diff {
        render_diff(diff, MAX_DIFF_PREVIEW_LINES, theme, out);
        return;
    }
    let Some(v) = parsed else {
        out.push(Line::styled(summary(&reply.content), theme.muted));
        return;
    };

    let count = |key: &str| v[key].as_u64().unwrap_or(0);
    let len = |key: &str| v[key].as_array().map_or(0, Vec::len);
    let suffix = truncated_suffix(v);
    let text = match tool {
        Some(BASH_TOOL) => return format_bash_result(v, theme, out),
        Some(READ_TOOL) => format!("{} lines{suffix}", count("total_lines")),
        Some(WRITE_TOOL) => format!("wrote {} bytes", count("bytes_written")),
        Some(LS_TOOL) => format!("{} entries", len("entries")),
        Some(FIND_TOOL) => format!("{} files", count("total")),
        Some(GREP_TOOL) => format!("{} matches{suffix}", count("matches")),
        Some("web_fetch") => format!("{} bytes{suffix}", count("total_bytes")),
        Some("web_search") => format!("{} results", len("results")),
        Some("skill") => format!("instructions loaded{suffix}"),
        Some("ask_user" | "task") => {
            format!(
                "answer: {}",
                summary(v["answer"].as_str().unwrap_or_default())
            )
        }
        _ => summary(&reply.content),
    };
    out.push(Line::styled(text, theme.muted));
}

/// The start of the output of a `bash` call, and how it ended.
fn format_bash_result(v: &Value, theme: Theme, out: &mut Vec<Line<'static>>) {
    if let Some(output) = v["output"].as_str() {
        let mut lines = output.lines();
        for line in lines.by_ref().take(MAX_BASH_PREVIEW_LINES) {
            out.push(output_line(line, theme.muted));
        }
        let extra = lines.count();
        if extra > 0 {
            out.push(Line::styled(
                format!("… +{extra} more lines (Ctrl-O to show)"),
                theme.muted,
            ));
        }
    }
    bash_notes(v, theme, out);
}

/// How a `bash` call ended, when not plainly.
fn bash_notes(v: &Value, theme: Theme, out: &mut Vec<Line<'static>>) {
    if let Some(code) = v["exit_code"].as_i64().filter(|&c| c != 0) {
        out.push(Line::styled(format!("exit {code}"), theme.error));
    }
    if v["timed_out"].as_bool() == Some(true) {
        out.push(Line::styled("timed out", theme.error));
    }
    if let Some(path) = v["output_file"].as_str() {
        out.push(Line::styled(format!("full output: {path}"), theme.muted));
    }
    if let Some(hosts) = v["refused_hosts"].as_array() {
        let hosts: Vec<_> = hosts.iter().filter_map(Value::as_str).collect();
        out.push(Line::styled(
            format!("refused hosts: {} (see --allow-host)", hosts.join(", ")),
            theme.error,
        ));
    }
}

fn truncated_suffix(v: &Value) -> &'static str {
    if v["truncated"].as_bool() == Some(true) {
        " (truncated)"
    } else {
        ""
    }
}

/// The whole of a result: the full diff, or the text the tool returned.
fn format_full_reply(
    reply: &ToolReply,
    parsed: Option<&Value>,
    theme: Theme,
    out: &mut Vec<Line<'static>>,
) {
    if let Some(diff) = &reply.diff {
        render_diff(diff, usize::MAX, theme, out);
        return;
    }
    let style = match parsed {
        Some(v) if v.get("error").is_some() => theme.error,
        _ => theme.muted,
    };
    let text = parsed
        .and_then(output_text)
        .unwrap_or(Cow::Borrowed(&reply.content));
    out.extend(text.lines().map(|line| output_line(line, style)));
    if let Some(v) = parsed.filter(|v| v.get("output").is_some()) {
        bash_notes(v, theme, out);
    }
}

/// The text a tool result carries: its error, output or content, its
/// entries or paths one per line, or else the whole result as indented JSON.
fn output_text(v: &Value) -> Option<Cow<'_, str>> {
    if let Some(text) = ["error", "output", "content"]
        .iter()
        .find_map(|k| v[k].as_str())
    {
        return Some(Cow::Borrowed(text));
    }
    if let Some(items) = ["entries", "paths"].iter().find_map(|k| v[k].as_array()) {
        let items: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
        return Some(Cow::Owned(items.join("\n")));
    }
    serde_json::to_string_pretty(v).ok().map(Cow::Owned)
}

/// Show up to `max_lines` lines of `diff`, each with its line number: the
/// old one for removed lines, the new one otherwise.
pub(crate) fn render_diff(
    diff: &Diff,
    max_lines: usize,
    theme: Theme,
    out: &mut Vec<Line<'static>>,
) {
    let last_line = diff
        .hunks
        .iter()
        .map(|h| (h.old_start + h.old_lines).max(h.new_start + h.new_lines) - 1)
        .max()
        .unwrap_or(0);
    let width = last_line.to_string().len();
    let mut emitted = 0usize;
    let mut omitted = 0usize;
    let mut push = |line: Line<'static>| {
        if emitted < max_lines {
            out.push(line);
            emitted += 1;
        } else {
            omitted += 1;
        }
    };
    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            push(Line::styled(format!("{:>width$}", "⋮"), theme.diff_hunk));
        }
        let mut old_line = hunk.old_start;
        let mut new_line = hunk.new_start;
        for line in &hunk.lines {
            let (number, sign, text, style) = match line {
                DiffLine::Context(s) => {
                    old_line += 1;
                    (post_increment(&mut new_line), ' ', s, theme.muted)
                }
                DiffLine::Removed(s) => (post_increment(&mut old_line), '-', s, theme.diff_remove),
                DiffLine::Added(s) => (post_increment(&mut new_line), '+', s, theme.diff_add),
            };
            push(Line::from(vec![
                Span::styled(format!("{number:>width$} "), theme.muted),
                Span::styled(format!("{sign}{}", expand_tabs(text)), style),
            ]));
        }
    }
    if omitted > 0 {
        out.push(Line::styled(
            format!("… +{omitted} more diff lines (Ctrl-O to show)"),
            theme.muted,
        ));
    } else if diff.truncated {
        out.push(Line::styled("… diff truncated", theme.muted));
    }
}

/// Return `n`, then add one to it.
fn post_increment(n: &mut usize) -> usize {
    *n += 1;
    *n - 1
}

/// Columns between the tab stops of tool output.
const TAB_WIDTH: usize = 4;

/// A line of tool output.
fn output_line(text: &str, style: Style) -> Line<'static> {
    Line::styled(expand_tabs(text).into_owned(), style)
}

/// `text` with its tabs turned into spaces, as the terminal drops them.
fn expand_tabs(text: &str) -> Cow<'_, str> {
    if !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + TAB_WIDTH);
    let mut col = 0;
    for ch in text.chars() {
        if ch == '\t' {
            let spaces = TAB_WIDTH - col % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', spaces));
            col += spaces;
        } else {
            out.push(ch);
            col += char_width(ch);
        }
    }
    Cow::Owned(out)
}

/// `secs` as the spinner and the tool calls show it, empty for 0.
pub(super) fn format_elapsed(secs: u64) -> String {
    if secs == 0 {
        String::new()
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// A tool call, with its arguments as JSON text.
#[derive(Debug, Clone)]
pub(crate) struct ToolCall {
    pub(super) name: String,
    pub(super) args: String,
}

/// What a tool returned.
#[derive(Debug, Clone)]
pub(crate) struct ToolReply {
    pub(super) content: String,
    /// The change of an `edit` call.
    pub(super) diff: Option<Diff>,
}

impl ToolReply {
    pub(super) fn new(content: String, details: Option<ToolDetails>) -> Self {
        // Only the diff shows: an image could be large.
        let diff = match details {
            Some(ToolDetails::Edit(diff)) => Some(diff),
            _ => None,
        };
        Self { content, diff }
    }
}

/// A tool call and how it ended.
#[derive(Debug, Clone)]
pub(crate) struct ToolRun {
    /// Unknown for a restored result whose call is missing.
    pub(super) call: Option<ToolCall>,
    /// Missing when the call never ended.
    pub(super) reply: Option<ToolReply>,
    /// How long it ran, unknown for a restored one.
    pub(super) elapsed: Option<Duration>,
}

impl ToolRun {
    /// Bytes of text it holds.
    pub(crate) fn size(&self) -> usize {
        let call = self.call.as_ref().map_or(0, |c| c.args.len());
        let reply = self.reply.as_ref().map_or(0, |r| {
            let diff = r.diff.iter().flat_map(|d| &d.hunks).flat_map(|h| &h.lines);
            r.content.len()
                + diff
                    .map(|line| match line {
                        DiffLine::Context(s) | DiffLine::Removed(s) | DiffLine::Added(s) => s.len(),
                    })
                    .sum::<usize>()
        });
        call + reply
    }

    /// A finished call of `name` that returned `content`.
    #[cfg(test)]
    pub(crate) fn finished(name: &str, content: String) -> Self {
        Self {
            call: Some(ToolCall {
                name: name.to_owned(),
                args: "{}".to_owned(),
            }),
            reply: Some(ToolReply {
                content,
                diff: None,
            }),
            elapsed: None,
        }
    }
}

/// Bounded ring buffer of live stdout/stderr lines from a running tool,
/// handling `\r` progress-bar overwrites without unbounded growth.
#[derive(Default)]
pub(super) struct ToolPreview {
    lines: VecDeque<String>,
    current: String,
    cr_pending: bool,
}

impl ToolPreview {
    pub(super) fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.current.is_empty()
    }

    pub(super) fn clear(&mut self) {
        self.lines.clear();
        self.current.clear();
        self.cr_pending = false;
    }

    pub(super) fn push(&mut self, chunk: &str) {
        for ch in chunk.chars() {
            match ch {
                '\n' => {
                    self.cr_pending = false;
                    let line = std::mem::take(&mut self.current);
                    if self.lines.len() >= MAX_LIVE_ROWS as usize {
                        self.lines.pop_front();
                    }
                    self.lines.push_back(line);
                }
                '\r' => self.cr_pending = true,
                _ => {
                    if self.cr_pending {
                        self.current.clear();
                        self.cr_pending = false;
                    }
                    if self.current.len() < MAX_TOOL_PREVIEW_LINE_BYTES {
                        self.current.push(ch);
                    }
                }
            }
        }
    }

    pub(super) fn render_lines(&self, style: Style) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(self.lines.len() + usize::from(!self.current.is_empty()));
        let indent = |text: &str| Line::styled(format!("  {}", expand_tabs(text)), style);
        for line in &self.lines {
            out.push(indent(line));
        }
        if !self.current.is_empty() {
            out.push(indent(&self.current));
        }
        out
    }
}
