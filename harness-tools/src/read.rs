// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::path::expand_tilde;

/// Default maximum number of lines returned by [`ReadTool::run`].
pub const DEFAULT_READ_LIMIT: usize = 2000;

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
    pub truncated: bool,
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
    /// A leading `~` in the path expands to `$HOME`. Files containing a NUL
    /// byte are rejected as binary; other invalid UTF-8 is replaced with
    /// U+FFFD.
    ///
    /// Lines keep their original endings (`\n`, `\r\n` or none on the last
    /// line), so reading a whole file returns its exact text.
    pub async fn run(self) -> Result<ReadOutput, ToolError> {
        let offset = self.offset.unwrap_or(1);
        if offset == 0 {
            return Err(ToolError::new("offset must be >= 1"));
        }
        let limit = self.limit.unwrap_or(DEFAULT_READ_LIMIT);
        if limit == 0 {
            return Err(ToolError::new("limit must be >= 1"));
        }

        let path = expand_tilde(&self.path);
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

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::io(&self.path, e))?;
        // Text files do not contain NUL bytes; binary content would only
        // reach the model as replacement characters.
        if bytes.contains(&0) {
            return Err(ToolError::new(format!("{}: binary file", self.path)));
        }
        let text = String::from_utf8_lossy(&bytes);

        // One pass: count the lines and find the byte range of the window,
        // without building a list of every line.
        let start = offset - 1;
        let end = start.saturating_add(limit);
        let (mut from, mut to) = (0, 0);
        let mut pos = 0;
        let mut total_lines = 0;
        for (i, line) in text.split_inclusive('\n').enumerate() {
            if i == start {
                from = pos;
            }
            pos += line.len();
            if i < end {
                to = pos;
            }
            total_lines += 1;
        }

        if total_lines == 0 {
            return Ok(ReadOutput {
                content: String::new(),
                total_lines: 0,
                truncated: false,
            });
        }
        if offset > total_lines {
            return Err(ToolError::new(format!(
                "{}: offset {offset} is beyond end of file ({total_lines} lines)",
                self.path
            )));
        }

        Ok(ReadOutput {
            content: text[from..to].to_owned(),
            total_lines,
            truncated: end < total_lines,
        })
    }
}
