use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::path::expand_tilde;

/// Default maximum number of lines returned by [`read`].
pub const DEFAULT_READ_LIMIT: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadParams {
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

/// Read a text file, optionally a window of lines.
///
/// A leading `~` in the path expands to `$HOME`. Files containing a NUL
/// byte are rejected as binary; other invalid UTF-8 is replaced with
/// U+FFFD.
///
/// Lines keep their original endings (`\n`, `\r\n` or none on the last
/// line), so reading a whole file returns its exact text.
pub async fn read(p: ReadParams) -> Result<ReadOutput, ToolError> {
    let offset = p.offset.unwrap_or(1);
    if offset == 0 {
        return Err(ToolError::new("offset must be >= 1"));
    }
    let limit = p.limit.unwrap_or(DEFAULT_READ_LIMIT);
    if limit == 0 {
        return Err(ToolError::new("limit must be >= 1"));
    }

    let path = expand_tilde(&p.path);
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| ToolError::io(&p.path, e))?;
    if meta.is_dir() {
        return Err(ToolError::new(format!("{}: is a directory", p.path)));
    }
    // FIFOs, sockets and devices can block forever or never end.
    if !meta.is_file() {
        return Err(ToolError::new(format!("{}: not a regular file", p.path)));
    }

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ToolError::io(&p.path, e))?;
    // Text files do not contain NUL bytes; binary content would only
    // reach the model as replacement characters.
    if bytes.contains(&0) {
        return Err(ToolError::new(format!("{}: binary file", p.path)));
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = lines.len();

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
            p.path
        )));
    }

    let start = offset - 1;
    let end = start.saturating_add(limit).min(total_lines);

    Ok(ReadOutput {
        content: lines[start..end].concat(),
        total_lines,
        truncated: end < total_lines,
    })
}
