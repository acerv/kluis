// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::Path;

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::path::resolve;

/// Default maximum number of entries returned by [`LsTool::run`].
pub const DEFAULT_LS_LIMIT: usize = 500;

/// Maximum bytes of entry names returned by [`LsTool::run`]; the list ends
/// at the last whole name that fits.
pub const MAX_LS_BYTES: usize = 50 * 1024;

/// A call of the `ls` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LsTool {
    /// Directory to list. Defaults to the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Maximum number of entries (>= 1). Defaults to [`DEFAULT_LS_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LsOutput {
    /// Entry names, sorted ignoring case; directories end in `/`, broken
    /// symlinks in `@`.
    pub entries: Vec<String>,
    /// Entries in the directory, including the ones left out.
    pub total: usize,
    /// True when the entry limit or [`MAX_LS_BYTES`] left entries out.
    pub truncated: bool,
}

/// What an entry of the directory is.
enum Kind {
    Dir,
    /// A symlink whose target is missing or loops.
    Broken,
    File,
}

impl LsTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":  { "type": "string" },
                "limit": { "type": "integer" }
            }
        })
    }

    /// The `ls` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new("ls")
            .with_description(
                "List a directory. Entries are sorted ignoring case and hidden \
                 files are included; directories end with '/'.",
            )
            .with_schema(Self::schema())
    }

    /// List a directory, dotfiles included, sorted by name ignoring case.
    ///
    /// A leading `~` in the path expands to `$HOME`; an empty path lists the
    /// current directory. Symlinks are followed, so a link to a directory
    /// ends in `/`; a link whose target is missing or loops ends in `@`. At
    /// most `limit` names and [`MAX_LS_BYTES`] bytes are returned, but
    /// `total` counts the whole directory.
    pub async fn run(self) -> Result<LsOutput, ToolError> {
        self.run_in(Path::new(".")).await
    }

    /// Like [`run`](Self::run), with relative paths, and the default one,
    /// starting at `cwd`.
    pub async fn run_in(self, cwd: &Path) -> Result<LsOutput, ToolError> {
        let limit = self.limit.unwrap_or(DEFAULT_LS_LIMIT);
        if limit == 0 {
            return Err(ToolError::new("limit must be >= 1"));
        }
        let given = match self.path.as_deref() {
            None | Some("") => ".",
            Some(path) => path,
        };
        let path = resolve(cwd, given);

        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::io(given, e))?;
        if !meta.is_dir() {
            return Err(ToolError::new(format!("{given}: not a directory")));
        }

        // Sorting needs every name, so read them all before cutting the
        // list down to `limit`.
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| ToolError::io(given, e))?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| ToolError::io(given, e))?
        {
            entries.push((entry.file_name(), entry.path()));
        }
        let total = entries.len();
        // Raw name first, so names that differ only in case keep an order
        // that does not depend on the file system.
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        entries.sort_by_cached_key(|(name, _)| name.to_string_lossy().to_lowercase());

        let mut names = Vec::new();
        let mut bytes = 0;
        for (name, entry) in entries {
            if names.len() >= limit {
                break;
            }
            let label = label(name, &entry).await;
            // One byte for the newline between names.
            let len = label.len() + usize::from(!names.is_empty());
            if bytes + len > MAX_LS_BYTES {
                break;
            }
            bytes += len;
            names.push(label);
        }
        let truncated = names.len() < total;

        Ok(LsOutput {
            entries: names,
            total,
            truncated,
        })
    }
}

/// Name of an entry, with the marker of its kind when it has one.
async fn label(name: OsString, path: &Path) -> String {
    let mut name = name.to_string_lossy().into_owned();
    match kind(path).await {
        Kind::Dir => name.push('/'),
        Kind::Broken => name.push('@'),
        Kind::File => {}
    }
    name
}

async fn kind(path: &Path) -> Kind {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => Kind::Dir,
        Ok(_) => Kind::File,
        // Only a missing or looping target makes a symlink broken; one we
        // may not look into, for instance, still points somewhere.
        Err(e) if is_dangling(&e) && is_symlink(path).await => Kind::Broken,
        Err(_) => Kind::File,
    }
}

/// Whether a symlink target cannot exist: it is missing, loops, or goes
/// through something that is not a directory.
fn is_dangling(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
        || err.raw_os_error() == Some(libc::ELOOP)
}

async fn is_symlink(path: &Path) -> bool {
    matches!(
        tokio::fs::symlink_metadata(path).await,
        Ok(meta) if meta.file_type().is_symlink()
    )
}
