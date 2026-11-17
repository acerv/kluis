// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::path::expand_tilde;

/// A call of the `write` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteTool {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOutput {
    pub bytes_written: usize,
}

impl WriteTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    /// The `write` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new("write")
            .with_description("Create or overwrite a file with the given content.")
            .with_schema(Self::schema())
    }

    /// Create or overwrite a file, creating missing parent directories.
    ///
    /// A leading `~` in the path expands to `$HOME`. Existing targets must be
    /// regular files (symlinks to regular files are followed); directories,
    /// FIFOs, devices and broken symlinks are rejected.
    pub async fn run(self) -> Result<WriteOutput, ToolError> {
        // Without this, the OS reports a confusing "Is a directory" error
        // even when nothing exists at the path.
        if self.path.ends_with('/') {
            return Err(ToolError::new(format!(
                "{}: path ends with '/', expected a file",
                self.path
            )));
        }
        let path = expand_tilde(&self.path);

        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_dir() => {
                return Err(ToolError::new(format!("{}: is a directory", self.path)));
            }
            // FIFOs, sockets and devices can block forever or have side effects.
            Ok(meta) if !meta.is_file() => {
                return Err(ToolError::new(format!("{}: not a regular file", self.path)));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // The path itself exists but its target does not.
                if tokio::fs::symlink_metadata(&path).await.is_ok() {
                    return Err(ToolError::new(format!("{}: broken symlink", self.path)));
                }
            }
            Err(e) => return Err(ToolError::io(&self.path, e)),
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ToolError::io(&self.path, e))?;
            }
        }

        tokio::fs::write(&path, self.content.as_bytes())
            .await
            .map_err(|e| ToolError::io(&self.path, e))?;

        Ok(WriteOutput {
            bytes_written: self.content.len(),
        })
    }
}
