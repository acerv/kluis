// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::path::resolve;
use crate::walk::stop_on_drop;

/// Default maximum number of lines returned by [`ReadTool::run`].
pub const DEFAULT_READ_LIMIT: usize = 500;

/// Maximum bytes returned by [`ReadTool::run`]; the window ends at the
/// last whole line that fits.
pub const MAX_READ_BYTES: usize = 24 * 1024;

const READ_CHUNK_BYTES: usize = 64 * 1024;

/// A call of the `read` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadTool {
    pub path: String,
    /// 1-indexed first line. Defaults to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Maximum number of lines (>= 1). Defaults to [`DEFAULT_READ_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadOutput {
    pub content: String,
    pub total_lines: usize,
    /// True when more lines exist after the returned window.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Offset of the first line after the window, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

impl ReadTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string" },
                "offset": { "type": "integer" },
                "limit":  { "type": "integer" }
            },
            "required": ["path"]
        })
    }

    /// The `read` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new("read")
            .with_description(
                "Read a text file. Use offset and limit to page through \
                 large files.",
            )
            .with_schema(Self::schema())
    }

    /// Read a text file, optionally a window of lines.
    ///
    /// The window holds at most `limit` lines and [`MAX_READ_BYTES`] bytes,
    /// and ends at a line boundary; a first line larger than that is an
    /// error. A leading `~` in the path expands to `$HOME`. Files containing
    /// a NUL byte are rejected as binary; other invalid UTF-8 is replaced
    /// with U+FFFD.
    ///
    /// Lines keep their original endings (`\n`, `\r\n` or none on the last
    /// line), so reading a whole file returns its exact text. A relative
    /// path starts at the current directory.
    pub async fn run(self) -> Result<ReadOutput, ToolError> {
        self.run_in(Path::new(".")).await
    }

    /// Like [`run`](Self::run), with relative paths starting at `cwd`.
    pub async fn run_in(self, cwd: &Path) -> Result<ReadOutput, ToolError> {
        let offset = self.offset.unwrap_or(1);
        if offset == 0 {
            return Err(ToolError::new("offset must be >= 1"));
        }
        let limit = self.limit.unwrap_or(DEFAULT_READ_LIMIT);
        if limit == 0 {
            return Err(ToolError::new("limit must be >= 1"));
        }

        let path = resolve(cwd, &self.path);
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::io(&self.path, e))?;
        if meta.is_dir() {
            return Err(ToolError::new(format!("{}: is a directory", self.path)));
        }
        // FIFOs, sockets and devices can block forever or never end.
        if !meta.is_file() {
            return Err(ToolError::new(format!("{}: not a regular file", self.path)));
        }

        // Stream in fixed chunks on a blocking thread so large files are
        // never loaded whole into memory, and stop if this future is dropped.
        let display = self.path;
        let (flag, stop) = stop_on_drop();
        let out =
            tokio::task::spawn_blocking(move || read_window(&path, &display, offset, limit, &flag))
                .await
                .map_err(|e| ToolError::new(format!("read: {e}")))??;
        drop(stop);
        Ok(out)
    }
}

fn read_window(
    path: &Path,
    display: &str,
    offset: usize,
    limit: usize,
    stop: &AtomicBool,
) -> Result<ReadOutput, ToolError> {
    let mut file = File::open(path).map_err(|e| ToolError::io(display, e))?;
    let start = offset - 1;
    let end = start.saturating_add(limit);

    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    let mut content = String::new();
    let mut line_buf = Vec::new();
    // First line left out because the window reached the byte limit.
    let mut cut_at = None;
    let mut line_idx = 0usize;
    let mut in_line = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(ToolError::new(format!("{display}: read cancelled")));
        }
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(ToolError::io(display, e)),
        };
        let chunk = &buf[..n];
        // Text files do not contain NUL bytes; binary content would only
        // reach the model as replacement characters.
        if memchr::memchr(0, chunk).is_some() {
            return Err(ToolError::new(format!("{display}: binary file")));
        }

        let mut slice = chunk;
        while !slice.is_empty() {
            if line_idx >= end || cut_at.is_some() {
                line_idx += memchr::memchr_iter(b'\n', slice).count();
                in_line = *slice.last().unwrap() != b'\n';
                break;
            }

            let (segment, ended) = match memchr::memchr(b'\n', slice) {
                Some(pos) => (&slice[..=pos], true),
                None => (slice, false),
            };
            slice = &slice[segment.len()..];

            if line_idx >= start {
                // `from_utf8_lossy` never shrinks byte length, so raw bytes
                // that already exceed the cap cannot fit after decoding.
                if content.len() + line_buf.len() + segment.len() > MAX_READ_BYTES {
                    cut_at = Some(line_idx);
                    line_buf = Vec::new();
                } else {
                    line_buf.extend_from_slice(segment);
                    if ended {
                        flush_line(&mut line_buf, &mut content, line_idx, &mut cut_at);
                    }
                }
            }

            if ended {
                line_idx += 1;
                in_line = false;
            } else {
                in_line = true;
            }
        }
    }

    if !line_buf.is_empty() {
        flush_line(&mut line_buf, &mut content, line_idx, &mut cut_at);
    }

    let total_lines = line_idx + usize::from(in_line);
    if total_lines == 0 {
        return Ok(ReadOutput {
            content: String::new(),
            total_lines: 0,
            truncated: false,
            next_offset: None,
        });
    }
    if offset > total_lines {
        return Err(ToolError::new(format!(
            "{display}: offset {offset} is beyond end of file ({total_lines} lines)"
        )));
    }
    if cut_at == Some(start) {
        return Err(ToolError::new(format!(
            "{display}: line {offset} is larger than {} KB; use bash to read part \
             of it, e.g. sed -n '{offset}p' {display} | head -c {MAX_READ_BYTES}",
            MAX_READ_BYTES / 1024
        )));
    }

    // Index of the first line after the window.
    let last = cut_at.unwrap_or(end.min(total_lines));
    let truncated = last < total_lines;
    Ok(ReadOutput {
        content,
        total_lines,
        truncated,
        next_offset: truncated.then_some(last + 1),
    })
}

fn flush_line(
    line_buf: &mut Vec<u8>,
    content: &mut String,
    line_idx: usize,
    cut_at: &mut Option<usize>,
) {
    let decoded = String::from_utf8_lossy(line_buf);
    if content.len() + decoded.len() > MAX_READ_BYTES {
        *cut_at = Some(line_idx);
    } else {
        content.push_str(&decoded);
    }
    line_buf.clear();
}
