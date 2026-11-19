// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs::{OpenOptions, Permissions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tempfile::Builder;

use crate::error::ToolError;
use crate::path::resolve;

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
    /// A leading `~` in the path expands to `$HOME`; a relative path starts
    /// at the current directory. Existing targets must be regular files
    /// (symlinks to regular files are followed); directories, FIFOs, devices
    /// and broken symlinks are rejected.
    pub async fn run(self) -> Result<WriteOutput, ToolError> {
        self.run_in(Path::new(".")).await
    }

    /// Like [`run`](Self::run), with relative paths starting at `cwd`.
    pub async fn run_in(self, cwd: &Path) -> Result<WriteOutput, ToolError> {
        // Without this, the OS reports a confusing "Is a directory" error
        // even when nothing exists at the path.
        if self.path.ends_with('/') {
            return Err(ToolError::new(format!(
                "{}: path ends with '/', expected a file",
                self.path
            )));
        }
        let path = resolve(cwd, &self.path);

        let existing_mode = match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_dir() => {
                return Err(ToolError::new(format!("{}: is a directory", self.path)));
            }
            // FIFOs, sockets and devices can block forever or have side effects.
            Ok(meta) if !meta.is_file() => {
                return Err(ToolError::new(format!("{}: not a regular file", self.path)));
            }
            Ok(meta) => Some(meta.permissions().mode()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // The path itself exists but its target does not.
                if tokio::fs::symlink_metadata(&path).await.is_ok() {
                    return Err(ToolError::new(format!("{}: broken symlink", self.path)));
                }
                None
            }
            Err(e) => return Err(ToolError::io(&self.path, e)),
        };

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ToolError::io(&self.path, e))?;
            }
        }

        // Pass the String by value: tokio moves it to the blocking thread
        // instead of copying a borrowed slice.
        let bytes_written = self.content.len();
        atomic_write(path, self.content, existing_mode)
            .await
            .map_err(|e| ToolError::io(&self.path, e))?;

        Ok(WriteOutput { bytes_written })
    }
}

/// Write `content` to `path` via a temporary file in the same directory and
/// an atomic rename, preserving `existing_mode` and following symlinks on
/// existing files.
pub(crate) async fn atomic_write(
    path: PathBuf,
    content: String,
    existing_mode: Option<u32>,
) -> io::Result<()> {
    tokio::task::spawn_blocking(move || atomic_write_sync(&path, content.as_bytes(), existing_mode))
        .await
        .map_err(io::Error::other)?
}

fn atomic_write_sync(path: &Path, content: &[u8], existing_mode: Option<u32>) -> io::Result<()> {
    let target = if existing_mode.is_some() {
        // Follow symlinks so rename updates the target rather than replacing
        // the symlink itself, and ensure the file itself is writable.
        let target = std::fs::canonicalize(path)?;
        OpenOptions::new().write(true).open(&target)?;
        target
    } else {
        path.to_path_buf()
    };

    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut builder = Builder::new();
    builder.prefix(".harness-tmp-");
    if existing_mode.is_none() {
        // Let open(2) apply the process umask instead of forcing 0o600.
        builder.permissions(Permissions::from_mode(0o666));
    }
    let mut tmp = match builder.tempfile_in(parent) {
        Ok(tmp) => tmp,
        // A single writable file inside a read-only folder (e.g. a bind mount)
        // cannot hold a sibling temp file; fall back to writing in place.
        Err(e)
            if existing_mode.is_some()
                && matches!(
                    e.kind(),
                    ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
                ) =>
        {
            return std::fs::write(&target, content);
        }
        Err(e) => return Err(e),
    };

    tmp.write_all(content)?;
    if let Some(mode) = existing_mode {
        // Keep permission bits, dropping setuid/setgid/sticky like write(2).
        tmp.as_file()
            .set_permissions(Permissions::from_mode(mode & 0o0777))?;
    }

    match tmp.persist(&target) {
        Ok(_) => Ok(()),
        // A writable single-file bind mount inside a writable folder rejects
        // rename with EBUSY; `target` was already verified writable above.
        Err(e) if existing_mode.is_some() && e.error.kind() == ErrorKind::ResourceBusy => {
            std::fs::write(&target, content)
        }
        Err(e) => Err(e.error),
    }
}
