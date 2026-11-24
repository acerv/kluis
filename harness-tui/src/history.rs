// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Persistent prompt history stored as JSON-encoded strings (one per line)
//! in `~/.kluis/history`, preserving multiline prompts cleanly.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Maximum entries retained in memory and after compaction.
pub const MAX_HISTORY: usize = 200;

/// Rewrite the file in place once it accumulates this many lines.
const COMPACT_THRESHOLD: usize = MAX_HISTORY * 2;

/// Maximum bytes read when loading the history file.
const MAX_HISTORY_BYTES: usize = 1024 * 1024;

/// Maximum byte length of a single persisted history entry.
const MAX_ENTRY_BYTES: usize = 16 * 1024;

/// Persistent append-only prompt history with periodic in-place compaction.
#[derive(Debug, Clone)]
pub struct History {
    path: PathBuf,
    entries: VecDeque<String>,
    total_lines: usize,
    needs_newline: bool,
}

impl History {
    /// Ensure `path` and its parent directory exist as a private (`0600`)
    /// regular file, so it can be bind-mounted writable inside a sandbox.
    pub fn ensure_file(path: &Path) -> io::Result<()> {
        ensure_parent(path)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if path.is_file() {
                    Ok(())
                } else {
                    Err(io::Error::other("history path is not a regular file"))
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Load existing prompt history from `path`, ignoring missing files and
    /// skipping malformed lines.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut history = Self {
            path,
            entries: VecDeque::new(),
            total_lines: 0,
            needs_newline: false,
        };
        let Some((raw, truncated_head)) = read_bounded(&history.path) else {
            return history;
        };
        history.needs_newline = !raw.is_empty() && !raw.ends_with(b"\n");
        let text = String::from_utf8_lossy(&raw);
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            history.total_lines += 1;
            let Ok(entry) = serde_json::from_str::<String>(trimmed) else {
                continue;
            };
            history.push_in_memory(&entry);
        }
        if truncated_head {
            history.total_lines = COMPACT_THRESHOLD;
        }
        history
    }

    /// Loaded entries from oldest to newest.
    pub fn entries(&self) -> Vec<String> {
        self.entries.iter().cloned().collect()
    }

    /// Record a submitted prompt in memory and on disk.
    ///
    /// Empty entries and consecutive duplicates are ignored.
    pub fn record(&mut self, entry: &str) -> io::Result<()> {
        let trimmed = entry.trim();
        if !self.push_in_memory(trimmed) {
            return Ok(());
        }
        if self.total_lines + 1 >= COMPACT_THRESHOLD {
            self.compact()
        } else {
            self.append(trimmed)
        }
    }

    fn push_in_memory(&mut self, entry: &str) -> bool {
        let trimmed = entry.trim();
        if trimmed.is_empty()
            || trimmed.len() > MAX_ENTRY_BYTES
            || self.entries.back().map(String::as_str) == Some(trimmed)
        {
            return false;
        }
        if self.entries.len() >= MAX_HISTORY {
            self.entries.pop_front();
        }
        self.entries.push_back(trimmed.to_owned());
        true
    }

    fn append(&mut self, entry: &str) -> io::Result<()> {
        ensure_parent(&self.path)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        let mut writer = BufWriter::new(file);
        if self.needs_newline {
            writer.write_all(b"\n")?;
        }
        write_entry(&mut writer, entry)?;
        writer.flush()?;
        self.needs_newline = false;
        self.total_lines += 1;
        Ok(())
    }

    /// Rewrite the history file in place (compatible with single-file bind
    /// mounts) keeping only the latest [`MAX_HISTORY`] entries.
    fn compact(&mut self) -> io::Result<()> {
        ensure_parent(&self.path)?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.path)?;
        let mut writer = BufWriter::new(file);
        for entry in &self.entries {
            write_entry(&mut writer, entry)?;
        }
        writer.flush()?;
        self.total_lines = self.entries.len();
        self.needs_newline = false;
        Ok(())
    }
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        harness_core::private_file::create_dir(parent)?;
    }
    Ok(())
}

fn write_entry(writer: &mut BufWriter<File>, entry: &str) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, entry)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    writer.write_all(b"\n")
}

fn read_bounded(path: &Path) -> Option<(Vec<u8>, bool)> {
    // Reject non-regular files (e.g. FIFOs) before opening so open() cannot block.
    if !path.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let len = meta.len();
    let truncated_head = len > MAX_HISTORY_BYTES as u64;
    if truncated_head {
        file.seek(SeekFrom::End(-(MAX_HISTORY_BYTES as i64))).ok()?;
    }
    let cap = usize::try_from(len)
        .unwrap_or(MAX_HISTORY_BYTES)
        .min(MAX_HISTORY_BYTES);
    let mut buf = Vec::with_capacity(cap);
    file.take(MAX_HISTORY_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    if truncated_head {
        // Drop the partial first line after seeking into the middle of the file.
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
        }
    }
    Some((buf, truncated_head))
}
