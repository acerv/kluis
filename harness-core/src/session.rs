// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Append-only JSONL session persistence scoped per workspace directory.
//!
//! Only Linux is supported.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hash::Hasher;
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::Utc;
use fnv::FnvHasher;
use genai::chat::ChatMessage;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Error;

/// Current JSONL session format version.
const FORMAT_VERSION: u32 = 1;

/// Maximum bytes read when loading a single session file.
pub const MAX_SESSION_BYTES: usize = 16 * 1024 * 1024;

/// One line in a `.jsonl` session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Header {
        version: u32,
        id: String,
        cwd: PathBuf,
        model: String,
        created_at: u64,
    },
    Model {
        model: String,
    },
    Message {
        message: ChatMessage,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
}

/// Borrowed mirror of [`SessionEntry`] so writing avoids cloning messages.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionEntryRef<'a> {
    Header {
        version: u32,
        id: &'a str,
        cwd: &'a Path,
        model: &'a str,
        created_at: u64,
    },
    Model {
        model: &'a str,
    },
    Message {
        message: &'a ChatMessage,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
}

/// Summary metadata of a persisted session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub model: String,
    pub created_at: u64,
}

/// Loaded session ready to resume in an [`crate::Agent`].
#[derive(Debug, Clone)]
pub struct SessionData {
    pub meta: SessionMeta,
    pub messages: Vec<ChatMessage>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// If the file had a truncated trailing line or lacked a final newline,
    /// the byte length of the valid prefix to repair before appending.
    repair_len: Option<u64>,
}

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
        create_dir_private(&self.dir)
    }

    /// Load the most recently modified session for this workspace, if any.
    pub fn latest(&self) -> Result<Option<SessionData>, Error> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
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
        let Some((_, _, path)) = candidates.into_iter().next() else {
            return Ok(None);
        };
        load_file(&path).map(Some)
    }

    /// Start a new lazy session writer; no file is created until the first
    /// message is appended.
    pub fn create(&self, model: impl Into<String>) -> SessionWriter {
        let (id, created_at) = new_session_id();
        let path = self.dir.join(format!("{id}.jsonl"));
        SessionWriter {
            id,
            path,
            cwd: self.cwd.clone(),
            model: model.into(),
            created_at,
            file: None,
            initialized: false,
            repair_len: None,
        }
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
            file: None,
            initialized: true,
            repair_len: data.repair_len,
        }
    }
}

/// Append-only writer for a single `.jsonl` session file.
#[derive(Debug)]
pub struct SessionWriter {
    id: String,
    path: PathBuf,
    cwd: PathBuf,
    model: String,
    created_at: u64,
    file: Option<BufWriter<File>>,
    initialized: bool,
    repair_len: Option<u64>,
}

impl SessionWriter {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append newly added conversation messages and flush immediately.
    pub fn append_messages(&mut self, messages: &[ChatMessage]) -> io::Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let writer = self.writer()?;
        for message in messages {
            write_entry(writer, &SessionEntryRef::Message { message })?;
        }
        writer.flush()
    }

    /// Record token usage for a completed turn (ignored if the session has no
    /// persisted messages yet).
    pub fn append_usage(&mut self, input_tokens: u64, output_tokens: u64) -> io::Result<()> {
        if !self.initialized || (input_tokens == 0 && output_tokens == 0) {
            return Ok(());
        }
        let writer = self.writer()?;
        write_entry(
            writer,
            &SessionEntryRef::Usage {
                input_tokens,
                output_tokens,
            },
        )?;
        writer.flush()
    }

    /// Record a model change. Before any message is written, this updates the
    /// pending header without creating an empty file.
    pub fn append_model(&mut self, model: impl Into<String>) -> io::Result<()> {
        let model = model.into();
        self.model = model.clone();
        if !self.initialized {
            return Ok(());
        }
        let writer = self.writer()?;
        write_entry(writer, &SessionEntryRef::Model { model: &model })?;
        writer.flush()
    }

    fn writer(&mut self) -> io::Result<&mut BufWriter<File>> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                create_dir_private(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&self.path)?;
            let mut repaired = false;
            if let Some(valid_len) = self.repair_len.take() {
                file.set_len(valid_len)?;
                repaired = valid_len > 0;
            }
            let mut buf = BufWriter::new(file);
            if repaired {
                // Ensure the repaired file ends on a newline boundary.
                buf.write_all(b"\n")?;
            }
            if !self.initialized {
                let header = SessionEntryRef::Header {
                    version: FORMAT_VERSION,
                    id: &self.id,
                    cwd: &self.cwd,
                    model: &self.model,
                    created_at: self.created_at,
                };
                write_entry(&mut buf, &header)?;
                self.initialized = true;
            }
            self.file = Some(buf);
        }
        Ok(self.file.as_mut().expect("session writer initialized"))
    }
}

fn create_dir_private(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

fn write_entry(writer: &mut BufWriter<File>, entry: &SessionEntryRef<'_>) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, entry)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    writer.write_all(b"\n")
}

fn load_file(path: &Path) -> Result<SessionData, Error> {
    if !path.is_file() {
        return Err(format!("{}: not a regular file", path.display()).into());
    }
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(format!("{}: not a regular file", path.display()).into());
    }
    if meta.len() > MAX_SESSION_BYTES as u64 {
        return Err(format!(
            "{}: session exceeds {} MiB limit",
            path.display(),
            MAX_SESSION_BYTES >> 20
        )
        .into());
    }

    let mut raw = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_SESSION_BYTES as u64 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > MAX_SESSION_BYTES {
        return Err(format!(
            "{}: session exceeds {} MiB limit",
            path.display(),
            MAX_SESSION_BYTES >> 20
        )
        .into());
    }

    let mut lines = split_byte_lines(&raw).enumerate().peekable();
    let Some((_, (first_bytes, first_end))) = lines.next() else {
        return Err(format!("{}: empty session file", path.display()).into());
    };
    let first = String::from_utf8_lossy(first_bytes);
    let SessionEntry::Header {
        version,
        id,
        cwd,
        mut model,
        created_at,
    } = serde_json::from_str(first.trim())
        .map_err(|e| format!("{}: invalid session header: {e}", path.display()))?
    else {
        return Err(format!("{}: missing session header", path.display()).into());
    };
    if version != FORMAT_VERSION {
        return Err(format!("{}: unsupported session version {version}", path.display()).into());
    }

    let mut messages = Vec::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut last_valid_end = first_end;
    let mut truncated_tail = false;

    while let Some((idx, (line_bytes, line_end))) = lines.next() {
        let is_last = lines.peek().is_none();
        let line = String::from_utf8_lossy(line_bytes);
        match serde_json::from_str::<SessionEntry>(line.trim()) {
            Ok(SessionEntry::Message { message }) => {
                messages.push(message);
                last_valid_end = line_end;
            }
            Ok(SessionEntry::Usage {
                input_tokens: inp,
                output_tokens: out,
            }) => {
                input_tokens = input_tokens.saturating_add(inp);
                output_tokens = output_tokens.saturating_add(out);
                last_valid_end = line_end;
            }
            Ok(SessionEntry::Model { model: next }) => {
                model = next;
                last_valid_end = line_end;
            }
            Ok(SessionEntry::Header { .. }) => {
                last_valid_end = line_end;
            }
            // Recover from a half-written trailing line left by a crash.
            Err(_) if is_last => {
                truncated_tail = true;
                break;
            }
            Err(e) => {
                return Err(
                    format!("{}:{}: corrupt session entry: {e}", path.display(), idx + 1).into(),
                )
            }
        }
    }

    let missing_newline = !raw.ends_with(b"\n");
    let repair_len = (truncated_tail || missing_newline).then_some(last_valid_end as u64);

    Ok(SessionData {
        meta: SessionMeta {
            id,
            path: path.to_path_buf(),
            cwd,
            model,
            created_at,
        },
        messages,
        input_tokens,
        output_tokens,
        repair_len,
    })
}

/// Iterate non-empty lines of `raw`, yielding `(line_bytes, end_offset_without_nl)`.
fn split_byte_lines(raw: &[u8]) -> impl Iterator<Item = (&[u8], usize)> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        while pos < raw.len() {
            let start = pos;
            let end = match memchr::memchr(b'\n', &raw[pos..]) {
                Some(rel) => {
                    pos += rel + 1;
                    start + rel
                }
                None => {
                    pos = raw.len();
                    raw.len()
                }
            };
            let slice = &raw[start..end];
            if slice.iter().any(|b| !b.is_ascii_whitespace()) {
                return Some((slice, end));
            }
        }
        None
    })
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
