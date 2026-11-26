// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Where the session files of a workspace are and how they are named.

use std::ffi::OsString;
use std::fs;
use std::hash::Hasher;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use fnv::FnvHasher;
use genai::chat::ChatMessage;
use uuid::Uuid;

use super::load::{load_file, summarize_file};
use super::{SessionData, SessionSummary, SessionWriter, TokenUsage};
use crate::{private_file, Error};

/// Manages `.jsonl` session files for one workspace folder under a root
/// directory (e.g. `~/.kluis/sessions`).
#[derive(Debug, Clone)]
pub struct SessionStore {
    cwd: PathBuf,
    dir: PathBuf,
}

impl SessionStore {
    /// Create a store scoped to `cwd` beneath `root`.
    pub fn new(root: impl Into<PathBuf>, cwd: &Path) -> Self {
        let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let dir = root.into().join(project_slug(&canonical));
        Self {
            cwd: canonical,
            dir,
        }
    }

    /// Directory holding `.jsonl` session files for this workspace.
    pub fn project_dir(&self) -> &Path {
        &self.dir
    }

    /// Ensure the project session directory exists with `0700` permissions.
    pub fn ensure_dir(&self) -> io::Result<()> {
        private_file::create_dir(&self.dir)
    }

    /// Load the most recently modified session for this workspace, if any.
    ///
    /// A file that does not load is skipped for the one before it, as
    /// [`Self::list`] does; when none loads, the error is the one of the
    /// most recent.
    pub fn latest(&self) -> Result<Option<SessionData>, Error> {
        let mut failed = None;
        for (_, _, path) in session_files(&self.dir)? {
            match load_file(&path) {
                Ok(data) => return Ok(Some(data)),
                Err(err) => failed = failed.or(Some(err)),
            }
        }
        failed.map_or(Ok(None), Err)
    }

    /// List valid persisted sessions for this workspace, newest first.
    ///
    /// Corrupt or unreadable files are skipped so one damaged file does not
    /// block resuming other sessions.
    pub fn list(&self) -> Result<Vec<SessionSummary>, Error> {
        let files = session_files(&self.dir)?;
        let mut out = Vec::with_capacity(files.len());
        for (modified, _, path) in files {
            if let Some(summary) = summarize_file(&path, modified) {
                out.push(summary);
            }
        }
        Ok(out)
    }

    /// Load a session by its exact ID or a unique ID prefix.
    pub fn load(&self, id: &str) -> Result<SessionData, Error> {
        let query = id.trim();
        if query.is_empty() {
            return Err("session id cannot be empty".into());
        }
        let files = session_files(&self.dir)?;
        let mut prefix_matches = Vec::new();
        for (_, _, path) in &files {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem == query {
                return load_file(path);
            }
            if stem.starts_with(query) {
                prefix_matches.push(path);
            }
        }
        match prefix_matches.as_slice() {
            [] => Err(format!("session not found: {query}").into()),
            [path] => load_file(path),
            many => Err(format!("ambiguous session id '{query}' ({} matches)", many.len()).into()),
        }
    }

    /// Start a new lazy session writer; no file is created until the first
    /// message is appended.
    pub fn create(&self, model: impl Into<String>) -> SessionWriter {
        let (id, created_at) = new_session_id();
        SessionWriter {
            path: self.session_path(&id),
            id,
            cwd: self.cwd.clone(),
            model: model.into(),
            created_at,
            forked_from: None,
            file: None,
            initialized: false,
            repair_len: None,
        }
    }

    /// Start a session holding a copy of the conversation of session
    /// `from`: its `messages`, token `usage` and the tokens its last
    /// request sent (`context`, 0 when unknown). The file of `from` is
    /// left as it is.
    pub fn fork(
        &self,
        from: &str,
        model: impl Into<String>,
        messages: &[ChatMessage],
        usage: TokenUsage,
        context: u64,
    ) -> io::Result<SessionWriter> {
        let mut writer = self.create(model);
        writer.forked_from = Some(from.to_owned());
        writer.append_messages(messages)?;
        writer.copy_diffs(&self.session_path(from), messages)?;
        writer.append_usage(usage, context)?;
        Ok(writer)
    }

    /// The file of session `id`.
    fn session_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// Open an existing loaded session so further turns append to the same
    /// file.
    pub fn open(&self, data: &SessionData) -> SessionWriter {
        SessionWriter {
            id: data.meta.id.clone(),
            path: data.meta.path.clone(),
            cwd: data.meta.cwd.clone(),
            model: data.meta.model.clone(),
            created_at: data.meta.created_at,
            forked_from: data.meta.forked_from.clone(),
            file: None,
            initialized: true,
            repair_len: data.repair_len,
        }
    }
}

/// Regular `.jsonl` files inside `dir`, sorted newest-first by mtime then name.
fn session_files(dir: &Path) -> io::Result<Vec<(SystemTime, OsString, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        candidates.push((modified, entry.file_name(), path));
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(candidates)
}

fn project_slug(path: &Path) -> String {
    let base: String = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("root")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "root" } else { base };
    let mut hasher = FnvHasher::default();
    hasher.write(path.as_os_str().as_bytes());
    format!("{base}-{:016x}", hasher.finish())
}

fn new_session_id() -> (String, u64) {
    let now = Utc::now();
    let secs = now.timestamp().max(0) as u64;
    let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), Uuid::new_v4());
    (id, secs)
}
