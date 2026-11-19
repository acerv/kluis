// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use genai::chat::Tool;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::path::resolve;
use crate::walk::{self, stop_on_drop};

/// Default maximum number of paths returned by [`FindTool::run`].
pub const DEFAULT_FIND_LIMIT: usize = 1000;

/// Maximum bytes of paths returned by [`FindTool::run`]; the list ends at
/// the last whole path that fits.
pub const MAX_FIND_BYTES: usize = 50 * 1024;

/// A call of the `find` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindTool {
    /// Glob pattern, e.g. `*.rs` or `src/**/*.rs`.
    pub pattern: String,
    /// Directory to search. Defaults to the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Maximum number of paths (>= 1). Defaults to [`DEFAULT_FIND_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindOutput {
    /// Matching paths relative to the searched directory, sorted ignoring
    /// case; directories end in `/`.
    pub paths: Vec<String>,
    /// Matching paths, including the ones left out.
    pub total: usize,
    /// True when the limit or [`MAX_FIND_BYTES`] left paths out.
    pub truncated: bool,
}

impl FindTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path":    { "type": "string" },
                "limit":   { "type": "integer" }
            },
            "required": ["pattern"]
        })
    }

    /// The `find` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new("find")
            .with_description(
                "Find files by glob pattern, e.g. '*.rs' or 'src/**/*.rs'. \
                 Respects .gitignore; directories end with '/'.",
            )
            .with_schema(Self::schema())
    }

    /// Find the files and directories below a directory matching a glob.
    ///
    /// A pattern without `/` matches names at any depth; one with `/`
    /// matches the path from the searched directory. `*` does not cross
    /// `/`, `**` does. The match ignores case unless the pattern has an
    /// uppercase letter.
    ///
    /// Hidden files are included, `.git` is not, and files ignored by
    /// `.gitignore`, `.ignore` or git's excludes are left out; outside a git
    /// repository `.gitignore` still applies. Symlinks are not followed.
    ///
    /// A leading `~` in the path expands to `$HOME`; an empty path searches
    /// the current directory. At most `limit` paths and [`MAX_FIND_BYTES`]
    /// bytes are returned, but `total` counts every match.
    pub async fn run(self) -> Result<FindOutput, ToolError> {
        self.run_in(Path::new(".")).await
    }

    /// Like [`run`](Self::run), with relative paths, and the default one,
    /// starting at `cwd`.
    pub async fn run_in(self, cwd: &Path) -> Result<FindOutput, ToolError> {
        let limit = self.limit.unwrap_or(DEFAULT_FIND_LIMIT);
        if limit == 0 {
            return Err(ToolError::new("limit must be >= 1"));
        }
        let pattern = self.pattern.strip_prefix("./").unwrap_or(&self.pattern);
        if pattern.is_empty() {
            return Err(ToolError::new("pattern must not be empty"));
        }
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .case_insensitive(!pattern.chars().any(char::is_uppercase))
            .build()
            .map_err(|e| ToolError::new(format!("invalid pattern: {e}")))?
            .compile_matcher();
        let by_path = pattern.contains('/');

        let given = match self.path.as_deref() {
            None | Some("") => ".",
            Some(path) => path,
        };
        let root = resolve(cwd, given);
        let meta = tokio::fs::metadata(&root)
            .await
            .map_err(|e| ToolError::io(given, e))?;
        if !meta.is_dir() {
            return Err(ToolError::new(format!("{given}: not a directory")));
        }

        // The walk blocks; stop it when the caller drops this future.
        let (flag, stop) = stop_on_drop();
        let mut paths = tokio::task::spawn_blocking(move || walk(&root, &glob, by_path, &flag))
            .await
            .map_err(|e| ToolError::new(format!("find: {e}")))?;
        drop(stop);

        let total = paths.len();
        // Raw order first, so paths that differ only in case keep an order
        // that does not depend on the walk.
        paths.sort();
        paths.sort_by_cached_key(|path| path.to_lowercase());

        let mut kept = Vec::new();
        let mut bytes = 0;
        for path in paths.into_iter().take(limit) {
            // One byte for the newline between paths.
            let len = path.len() + usize::from(!kept.is_empty());
            if bytes + len > MAX_FIND_BYTES {
                break;
            }
            bytes += len;
            kept.push(path);
        }
        let truncated = kept.len() < total;

        Ok(FindOutput {
            paths: kept,
            total,
            truncated,
        })
    }
}

/// Paths below `root` matching `glob`, relative to `root`, in no order.
fn walk(root: &Path, glob: &GlobMatcher, by_path: bool, stop: &AtomicBool) -> Vec<String> {
    let (tx, rx) = mpsc::channel();
    walk::builder(root).build_parallel().run(|| {
        let tx = tx.clone();
        Box::new(move |entry| {
            if stop.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            // Folders we may not read, and races with deletions.
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            let Ok(rel) = entry.path().strip_prefix(root) else {
                return WalkState::Continue;
            };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            let hit = if by_path {
                glob.is_match(rel)
            } else {
                glob.is_match(entry.file_name())
            };
            if hit {
                let mut path = rel.to_string_lossy().into_owned();
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    path.push('/');
                }
                let _ = tx.send(path);
            }
            WalkState::Continue
        })
    });
    drop(tx);
    rx.into_iter().collect()
}
