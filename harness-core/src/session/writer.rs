// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Writing a session file.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use genai::chat::{ChatMessage, ContentPart};
use serde::{Deserialize, Serialize};

use super::{answered_calls, TokenUsage, FORMAT_VERSION};
use crate::private_file;
use crate::tools::Diff;
use crate::Thinking;

/// Writer for a single `.jsonl` session file, which only grows but on
/// compaction.
#[derive(Debug)]
pub struct SessionWriter {
    pub(super) id: String,
    pub(super) path: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) model: String,
    pub(super) created_at: u64,
    pub(super) forked_from: Option<String>,
    pub(super) thinking: Option<Thinking>,
    pub(super) file: Option<BufWriter<File>>,
    pub(super) initialized: bool,
    pub(super) repair_len: Option<u64>,
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
            let message = &without_images(message);
            write_entry(writer, &SessionEntryRef::Message { message })?;
        }
        writer.flush()
    }

    /// Record the change the `edit` call `call_id` made.
    pub fn append_diff(&mut self, call_id: &str, diff: &Diff) -> io::Result<()> {
        let writer = self.writer()?;
        write_entry(writer, &SessionEntryRef::Diff { call_id, diff })?;
        writer.flush()
    }

    /// Copy the changes of the `edit` calls of `messages` from the session
    /// file at `from`.
    pub(super) fn copy_diffs(&mut self, from: &Path, messages: &[ChatMessage]) -> io::Result<()> {
        let calls = answered_calls(messages);
        if calls.is_empty() {
            return Ok(());
        }
        let writer = self.writer()?;
        copy_lines(from, writer, |entry| {
            entry.kind == "diff" && entry.answers(&calls)
        })?;
        writer.flush()
    }

    /// Record that `messages` replace the conversation, as after a
    /// compaction or when a prompt is sent again.
    ///
    /// The file is rewritten without the messages they replace, but the
    /// first one, which names the session in the list, so that long sessions
    /// stay small enough to load.
    pub fn append_compact(&mut self, messages: &[ChatMessage]) -> io::Result<()> {
        if messages.is_empty() && !self.initialized {
            return Ok(());
        }
        // The header, and the end of a file cut by a crash repaired.
        self.writer()?.flush()?;
        self.file = None;
        rewrite_compacted(&self.path, messages)
    }

    /// Record the token usage of a prompt and the tokens its last request
    /// sent, 0 when unknown (ignored if the session has no persisted messages
    /// yet).
    pub fn append_usage(&mut self, usage: TokenUsage, context: u64) -> io::Result<()> {
        if !self.initialized || usage.is_empty() {
            return Ok(());
        }
        let writer = self.writer()?;
        write_entry(
            writer,
            &SessionEntryRef::Usage {
                input_tokens: usage.input,
                output_tokens: usage.output,
                cached_tokens: usage.cached,
                context_tokens: context,
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

    /// Record `thinking` in the header of the file it creates.
    pub fn with_thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// Record a thinking effort change. Before any message is written, this
    /// updates the pending header without creating an empty file.
    pub fn append_thinking(&mut self, thinking: Thinking) -> io::Result<()> {
        self.thinking = Some(thinking);
        if !self.initialized {
            return Ok(());
        }
        let writer = self.writer()?;
        write_entry(writer, &SessionEntryRef::Thinking { thinking })?;
        writer.flush()
    }

    fn writer(&mut self) -> io::Result<&mut BufWriter<File>> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                private_file::create_dir(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&self.path)?;
            let mut repaired = false;
            if let Some(valid_len) = self.repair_len {
                file.set_len(valid_len)?;
                // Only now: a failure leaves it to the next write.
                self.repair_len = None;
                repaired = valid_len > 0;
            }
            // A file that was removed since it was loaded starts again.
            let empty = file.metadata()?.len() == 0;
            let mut buf = BufWriter::new(file);
            if repaired {
                // Ensure the repaired file ends on a newline boundary.
                buf.write_all(b"\n")?;
            }
            if !self.initialized || empty {
                let header = SessionEntryRef::Header {
                    version: FORMAT_VERSION,
                    id: &self.id,
                    cwd: &self.cwd,
                    model: &self.model,
                    created_at: self.created_at,
                    forked_from: self.forked_from.as_deref(),
                    thinking: self.thinking,
                };
                write_entry(&mut buf, &header)?;
                self.initialized = true;
            }
            self.file = Some(buf);
        }
        Ok(self.file.as_mut().expect("session writer initialized"))
    }
}

/// The `type` of a session file line, and the tool call it is about.
#[derive(Deserialize)]
struct EntryKind {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    call_id: Option<String>,
}

impl EntryKind {
    /// Whether the line is about one of `calls`.
    fn answers(&self, calls: &HashSet<&str>) -> bool {
        self.call_id.as_deref().is_some_and(|id| calls.contains(id))
    }
}

/// Write to `out` the lines of the session file at `path` that `keep`
/// picks. Corrupt lines, as one cut by a crash, are left out.
fn copy_lines(
    path: &Path,
    out: &mut impl Write,
    mut keep: impl FnMut(&EntryKind) -> bool,
) -> io::Result<()> {
    let old = BufReader::new(File::open(path)?);
    for line in old.split(b'\n') {
        let line = line?;
        if serde_json::from_slice::<EntryKind>(&line).is_ok_and(|entry| keep(&entry)) {
            out.write_all(&line)?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Rewrite the session file at `path` with what loading it still needs, its
/// header, first message, model changes, token usage and the diffs of the
/// calls of `messages`, then `messages` replacing the conversation. The new
/// file replaces the old one only once complete.
fn rewrite_compacted(path: &Path, messages: &[ChatMessage]) -> io::Result<()> {
    private_file::replace(path, |tmp| write_compacted(path, tmp, messages))
}

fn write_compacted(path: &Path, tmp: &mut File, messages: &[ChatMessage]) -> io::Result<()> {
    let calls = answered_calls(messages);
    let mut out = BufWriter::new(tmp);
    let mut first_message = true;
    copy_lines(path, &mut out, |entry| match entry.kind.as_str() {
        "message" => std::mem::take(&mut first_message),
        "compact" => false,
        "diff" => entry.answers(&calls),
        _ => true,
    })?;
    write_entry(&mut out, &SessionEntryRef::Compact { messages })?;
    out.flush()
}

/// `message` with its images replaced by a note: they would soon make the
/// file too large to load.
fn without_images(message: &ChatMessage) -> Cow<'_, ChatMessage> {
    if !message.content.contains_binary() {
        return Cow::Borrowed(message);
    }
    let mut message = message.clone();
    for part in message.content.iter_mut() {
        if matches!(part, ContentPart::Binary(_)) {
            *part = ContentPart::Text("[image not saved]".to_owned());
        }
    }
    Cow::Owned(message)
}

fn write_entry(writer: &mut impl Write, entry: &SessionEntryRef<'_>) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, entry)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    writer.write_all(b"\n")
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
        #[serde(skip_serializing_if = "Option::is_none")]
        forked_from: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<Thinking>,
    },
    Model {
        model: &'a str,
    },
    Thinking {
        thinking: Thinking,
    },
    Message {
        message: &'a ChatMessage,
    },
    Compact {
        messages: &'a [ChatMessage],
    },
    Diff {
        call_id: &'a str,
        diff: &'a Diff,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        context_tokens: u64,
    },
}
