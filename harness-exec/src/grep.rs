// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use genai::chat::Tool;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::WalkState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::path::resolve;
use crate::walk::{self, stop_on_drop};

/// Name of the `grep` tool.
pub const GREP_TOOL: &str = "grep";

/// Default maximum number of matching lines returned by [`GrepTool::run`].
pub const DEFAULT_GREP_LIMIT: usize = 50;

/// Maximum bytes of output returned by [`GrepTool::run`]; the output ends
/// at the last whole line that fits.
pub const MAX_GREP_BYTES: usize = 16 * 1024;

/// Characters kept from a long line.
pub const MAX_GREP_LINE_CHARS: usize = 250;

/// A call of the `grep` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepTool {
    /// Regular expression, or text with `literal`.
    pub pattern: String,
    /// Directory or file to search. Defaults to the current directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Only search files matching this glob, or not matching it with a
    /// leading `!`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_case: Option<bool>,
    /// Match the pattern as plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<bool>,
    /// Lines shown before and after each match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
    /// Maximum number of matching lines (>= 1). Defaults to
    /// [`DEFAULT_GREP_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepOutput {
    /// One line per result: `path:line: text` for matching lines,
    /// `path-line- text` for context lines.
    pub output: String,
    /// Matching lines in the output.
    pub matches: usize,
    /// True when the limit or [`MAX_GREP_BYTES`] left matches out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl GrepTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern":    { "type": "string" },
                "path":       { "type": "string" },
                "glob":       { "type": "string" },
                "ignoreCase": { "type": "boolean" },
                "literal":    { "type": "boolean" },
                "context":    { "type": "integer" },
                "limit":      { "type": "integer" }
            },
            "required": ["pattern"]
        })
    }

    /// The `grep` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new(GREP_TOOL)
            .with_description(
                "Search file contents for a regex, or plain text with literal. \
                 Returns 'path:line: text' lines. Respects .gitignore.",
            )
            .with_schema(Self::schema())
    }

    /// Search the lines of a file, or of the files below a directory.
    ///
    /// Files are walked like [`FindTool`](crate::FindTool) does: hidden
    /// files are searched, `.git` and ignored files are not. `glob` keeps
    /// the files matching it, or drops them with a leading `!`; a glob
    /// without `/` matches names at any depth. Binary files are skipped.
    ///
    /// Files are searched in parallel, like ripgrep does, and the search
    /// stops once enough matches are found, so with a limit the matches
    /// kept depend on timing. Results are grouped by file, files sorted
    /// ignoring case, and lines in file order. Paths are relative to the
    /// searched directory; a searched file shows as given. At most `limit`
    /// matching lines are returned, with their context, and at most
    /// [`MAX_GREP_BYTES`]; lines longer than [`MAX_GREP_LINE_CHARS`] are
    /// cut.
    ///
    /// A leading `~` in the path expands to `$HOME`; an empty path searches
    /// the current directory.
    pub async fn run(self) -> Result<GrepOutput, ToolError> {
        self.run_in(Path::new(".")).await
    }

    /// Like [`run`](Self::run), with relative paths, and the default one,
    /// starting at `cwd`.
    pub async fn run_in(self, cwd: &Path) -> Result<GrepOutput, ToolError> {
        let limit = self.limit.unwrap_or(DEFAULT_GREP_LIMIT);
        if limit == 0 {
            return Err(ToolError::new("limit must be >= 1"));
        }
        let context = self.context.unwrap_or(0);
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(self.ignore_case.unwrap_or(false))
            .fixed_strings(self.literal.unwrap_or(false))
            .line_terminator(Some(b'\n'))
            .build(&self.pattern)
            .map_err(|e| ToolError::new(format!("invalid pattern: {e}")))?;

        let given = match self.path.as_deref() {
            None | Some("") => ".",
            Some(path) => path,
        };
        let root = resolve(cwd, given);
        let meta = tokio::fs::metadata(&root)
            .await
            .map_err(|e| ToolError::io(given, e))?;
        let only_file = !meta.is_dir();
        // FIFOs, sockets and devices can block forever or never end.
        if only_file && !meta.is_file() {
            return Err(ToolError::new(format!("{given}: not a regular file")));
        }
        let overrides = match self.glob.as_deref() {
            Some(glob) if !glob.is_empty() && !only_file => Some(overrides(&root, glob)?),
            _ => None,
        };
        let given = given.to_owned();

        // The search blocks; stop it when the caller drops this future.
        let (flag, stop) = stop_on_drop();
        let out = tokio::task::spawn_blocking(move || {
            if only_file {
                let mut merge = Merge::new(limit, context);
                let stopped = || flag.load(Ordering::Relaxed);
                let mut searcher = searcher(context);
                let lines = search_file(&mut searcher, &matcher, &root, context, limit, &stopped);
                merge.add(given, lines);
                return merge.finish();
            }
            search(&root, overrides, &matcher, context, limit, &flag)
        })
        .await
        .map_err(|e| ToolError::new(format!("grep: {e}")))?;
        drop(stop);
        Ok(out)
    }
}

/// The override keeping, or with `!` dropping, the files matching `glob`.
fn overrides(root: &Path, glob: &str) -> Result<Override, ToolError> {
    let (not, mut glob) = match glob.strip_prefix('!') {
        Some(rest) => ("!", rest),
        None => ("", glob),
    };
    // Paths below the root never start with `./`.
    while let Some(rest) = glob.strip_prefix("./") {
        glob = rest;
    }
    let mut builder = OverrideBuilder::new(root);
    builder
        .add(&format!("{not}{glob}"))
        .and_then(|b| b.build())
        .map_err(|e| ToolError::new(format!("invalid glob: {e}")))
}

/// A line found in a file.
struct Line {
    number: u64,
    text: String,
    is_match: bool,
}

/// A searcher for one thread.
fn searcher(context: usize) -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .binary_detection(BinaryDetection::quit(0))
        .build()
}

/// Search the files below `root` like ripgrep: the walker threads search
/// each file they find, and the walk stops once enough matches are found.
fn search(
    root: &Path,
    overrides: Option<Override>,
    matcher: &RegexMatcher,
    context: usize,
    limit: usize,
    cancel: &AtomicBool,
) -> GrepOutput {
    let mut builder = walk::builder(root);
    if let Some(overrides) = overrides {
        builder.overrides(overrides);
    }
    let done = AtomicBool::new(false);
    let stopped = || cancel.load(Ordering::Relaxed) || done.load(Ordering::Relaxed);
    let mut merge = Merge::new(limit, context);

    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            builder.build_parallel().run(|| {
                let tx = tx.clone();
                let mut searcher = searcher(context);
                Box::new(move |entry| {
                    if stopped() {
                        return WalkState::Quit;
                    }
                    // Folders we may not read, and races with deletions.
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let path = entry.path();
                    let lines = search_file(&mut searcher, matcher, path, context, limit, &stopped);
                    let Ok(rel) = path.strip_prefix(root) else {
                        return WalkState::Continue;
                    };
                    if !lines.is_empty()
                        && tx
                            .send((rel.to_string_lossy().into_owned(), lines))
                            .is_err()
                    {
                        return WalkState::Quit;
                    }
                    WalkState::Continue
                })
            });
        });

        for (name, lines) in rx {
            if !merge.add(name, lines) {
                done.store(true, Ordering::Relaxed);
                break;
            }
        }
    });
    merge.finish()
}

/// The lines of a file a report may show: every line until `limit + 1`
/// matches, then the context of the last match. Unreadable files have none.
fn search_file(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
    context: usize,
    limit: usize,
    stopped: &dyn Fn() -> bool,
) -> Vec<Line> {
    let mut sink = Collect {
        lines: Vec::new(),
        matches: 0,
        limit,
        context: context as u64,
        window_end: 0,
        stopped,
    };
    match searcher.search_path(matcher, path, &mut sink) {
        Ok(()) => sink.lines,
        Err(_) => Vec::new(),
    }
}

/// Keeps the lines of one file.
struct Collect<'a> {
    lines: Vec<Line>,
    matches: usize,
    limit: usize,
    context: u64,
    /// Last line of the context of the `limit`-th match.
    window_end: u64,
    stopped: &'a dyn Fn() -> bool,
}

impl Collect<'_> {
    /// Keep a line; false once later lines cannot be shown.
    fn push(&mut self, number: u64, bytes: &[u8], is_match: bool) -> bool {
        if (self.stopped)() || (self.matches > self.limit && number > self.window_end) {
            return false;
        }
        self.lines.push(Line {
            number,
            text: line_text(bytes),
            is_match,
        });
        if is_match {
            self.matches += 1;
            if self.matches == self.limit {
                self.window_end = number + self.context;
            }
        }
        true
    }
}

impl Sink for Collect<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        Ok(self.push(mat.line_number().unwrap_or(0), mat.bytes(), true))
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        Ok(self.push(ctx.line_number().unwrap_or(0), ctx.bytes(), false))
    }
}

/// A line without its terminator, cut to [`MAX_GREP_LINE_CHARS`].
fn line_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes);
    match text.char_indices().nth(MAX_GREP_LINE_CHARS) {
        Some((end, _)) => format!("{}... [truncated]", &text[..end]),
        None => text.into_owned(),
    }
}

/// Builds the report from the lines of each file, in the order the files
/// are searched.
struct Merge {
    limit: usize,
    context: u64,
    /// Name and output lines of each file shown.
    blocks: Vec<(String, String)>,
    /// Size of the output, newlines between lines included.
    bytes: usize,
    matches: usize,
    truncated: bool,
}

impl Merge {
    fn new(limit: usize, context: usize) -> Self {
        Self {
            limit,
            context: context as u64,
            blocks: Vec::new(),
            bytes: 0,
            matches: 0,
            truncated: false,
        }
    }

    /// Report the lines of a file; false once nothing more can be shown.
    fn add(&mut self, name: String, lines: Vec<Line>) -> bool {
        let mut block = String::new();
        let more = self.fill(&mut block, &name, lines);
        if !block.is_empty() {
            self.blocks.push((name, block));
        }
        more
    }

    fn fill(&mut self, block: &mut String, name: &str, lines: Vec<Line>) -> bool {
        // Last line of the context of the last match shown, in this file.
        let mut window_end = None;
        for line in lines {
            if self.matches < self.limit {
                if !self.emit(block, name, &line, line.is_match) {
                    return false;
                }
                if line.is_match {
                    self.matches += 1;
                    if self.matches == self.limit {
                        window_end = Some(line.number + self.context);
                    }
                }
                continue;
            }
            // Past the limit: one more match proves results were left out,
            // and the context of the last match shown still shows.
            if line.is_match {
                self.truncated = true;
            }
            match window_end {
                Some(end) if line.number <= end => {
                    if !self.emit(block, name, &line, false) {
                        return false;
                    }
                }
                _ if self.truncated => return false,
                _ => {}
            }
        }
        !(self.matches == self.limit && self.truncated)
    }

    /// Append a line; false when it does not fit in [`MAX_GREP_BYTES`].
    fn emit(&mut self, block: &mut String, name: &str, line: &Line, is_match: bool) -> bool {
        let sep = if is_match { ':' } else { '-' };
        let text = format!("{name}{sep}{}{sep} {}", line.number, line.text);
        // One byte for the newline between lines.
        let len = text.len() + usize::from(self.bytes > 0);
        if self.bytes + len > MAX_GREP_BYTES {
            self.truncated = true;
            return false;
        }
        self.bytes += len;
        if !block.is_empty() {
            block.push('\n');
        }
        block.push_str(&text);
        true
    }

    /// The report, files sorted ignoring case.
    fn finish(mut self) -> GrepOutput {
        // Raw order first, so names that differ only in case keep an order
        // that does not depend on the search.
        self.blocks.sort();
        self.blocks
            .sort_by_cached_key(|(name, _)| name.to_lowercase());
        let blocks: Vec<_> = self.blocks.into_iter().map(|(_, block)| block).collect();
        GrepOutput {
            output: blocks.join("\n"),
            matches: self.matches,
            truncated: self.truncated,
        }
    }
}
