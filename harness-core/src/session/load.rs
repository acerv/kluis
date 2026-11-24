// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Reading a session file, whole or only its summary.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use genai::chat::ChatRole;
use serde::de::IgnoredAny;
use serde::Deserialize;

use super::{
    SessionData, SessionEntry, SessionMeta, SessionSummary, TokenUsage, FORMAT_VERSION,
    MAX_SESSION_BYTES,
};
use crate::Error;

/// Maximum characters stored for [`SessionSummary::first_prompt`].
const MAX_PROMPT_PREVIEW_CHARS: usize = 120;

/// The lines of a session file that hold something, read one at a time
/// up to [`MAX_SESSION_BYTES`].
struct Lines<'a> {
    path: &'a Path,
    reader: BufReader<io::Take<File>>,
    buf: Vec<u8>,
    /// Bytes read so far.
    read: usize,
    /// Lines given so far.
    count: usize,
    /// Whether the last line read, blank or not, ends with a newline. The
    /// one of a file cut by a crash does not.
    newline: bool,
}

impl<'a> Lines<'a> {
    fn open(path: &'a Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Err(format!("{}: not a regular file", path.display()).into());
        }
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(format!("{}: not a regular file", path.display()).into());
        }
        let lines = Self {
            path,
            reader: BufReader::new(file.take(MAX_SESSION_BYTES as u64 + 1)),
            buf: Vec::new(),
            read: 0,
            count: 0,
            newline: true,
        };
        if meta.len() > MAX_SESSION_BYTES as u64 {
            return Err(lines.too_large());
        }
        Ok(lines)
    }

    fn too_large(&self) -> Error {
        format!(
            "{}: session exceeds {} MiB limit",
            self.path.display(),
            MAX_SESSION_BYTES >> 20
        )
        .into()
    }

    /// The next line that is not blank, and where it ends in the file,
    /// its newline left out.
    fn next_line(&mut self) -> Result<Option<(Cow<'_, str>, usize)>, Error> {
        loop {
            self.buf.clear();
            let n = self.reader.read_until(b'\n', &mut self.buf)?;
            if n == 0 {
                return Ok(None);
            }
            self.read += n;
            self.newline = self.buf.ends_with(b"\n");
            if self.read > MAX_SESSION_BYTES {
                return Err(self.too_large());
            }
            if self.buf.iter().any(|b| !b.is_ascii_whitespace()) {
                break;
            }
        }
        self.count += 1;
        let end = self.read - usize::from(self.newline);
        Ok(Some((String::from_utf8_lossy(&self.buf), end)))
    }

    /// The header the file starts with, and where it ends.
    fn header(&mut self) -> Result<(SessionMeta, usize), Error> {
        let path = self.path;
        let Some((line, end)) = self.next_line()? else {
            return Err(format!("{}: empty session file", path.display()).into());
        };
        let SessionEntry::Header {
            version,
            id,
            cwd,
            model,
            created_at,
        } = serde_json::from_str(line.trim())
            .map_err(|e| format!("{}: invalid session header: {e}", path.display()))?
        else {
            return Err(format!("{}: missing session header", path.display()).into());
        };
        if version != FORMAT_VERSION {
            return Err(
                format!("{}: unsupported session version {version}", path.display()).into(),
            );
        }
        let meta = SessionMeta {
            id,
            path: path.to_path_buf(),
            cwd,
            model,
            created_at,
        };
        Ok((meta, end))
    }
}

/// Zero-allocation entry view used by [`summarize_file`] once `first_prompt`
/// has already been extracted, avoiding full [`ChatMessage`] deserialization
/// for the rest of the file while still validating JSON structure.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SummaryEntryLight {
    Header {},
    Model {
        model: String,
    },
    Message {
        #[allow(dead_code)]
        message: IgnoredAny,
    },
    Compact {
        messages: Vec<IgnoredAny>,
    },
    Usage {},
}

/// Stream a single session file line by line to extract its [`SessionSummary`]
/// without retaining full message payloads in memory.
pub(super) fn summarize_file(path: &Path, modified: SystemTime) -> Option<SessionSummary> {
    let mut lines = Lines::open(path).ok()?;
    let (meta, _) = lines.header().ok()?;
    let SessionMeta {
        id,
        mut model,
        created_at,
        ..
    } = meta;

    let mut message_count = 0usize;
    let mut first_prompt = None;
    let mut saw_error = false;

    while let Some((line, _)) = lines.next_line().ok()? {
        // Only a corrupt final line (crash tail) is recoverable.
        if saw_error {
            return None;
        }
        let trimmed = line.trim();
        if first_prompt.is_some() {
            match serde_json::from_str::<SummaryEntryLight>(trimmed) {
                Ok(SummaryEntryLight::Message { .. }) => message_count += 1,
                Ok(SummaryEntryLight::Compact { messages }) => message_count = messages.len(),
                Ok(SummaryEntryLight::Model { model: next }) => model = next,
                Ok(SummaryEntryLight::Usage {} | SummaryEntryLight::Header {}) => {}
                Err(_) => saw_error = true,
            }
        } else {
            match serde_json::from_str::<SessionEntry>(trimmed) {
                Ok(SessionEntry::Message { message }) => {
                    message_count += 1;
                    if message.role == ChatRole::User {
                        if let Some(text) = message.content.joined_texts() {
                            let preview = prompt_preview(&text);
                            if !preview.is_empty() {
                                first_prompt = Some(preview);
                            }
                        }
                    }
                }
                Ok(SessionEntry::Compact { messages }) => {
                    message_count = messages.len();
                }
                Ok(SessionEntry::Model { model: next }) => model = next,
                Ok(SessionEntry::Usage { .. } | SessionEntry::Header { .. }) => {}
                Err(_) => saw_error = true,
            }
        }
    }

    let updated_at = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(created_at);

    Some(SessionSummary {
        id,
        path: path.to_path_buf(),
        model,
        created_at,
        updated_at,
        message_count,
        first_prompt,
    })
}

fn prompt_preview(text: &str) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for word in text.split_whitespace() {
        let need_space = !out.is_empty();
        if count + usize::from(need_space) >= MAX_PROMPT_PREVIEW_CHARS {
            out.push('…');
            return out;
        }
        if need_space {
            out.push(' ');
            count += 1;
        }
        for ch in word.chars() {
            if count >= MAX_PROMPT_PREVIEW_CHARS {
                out.push('…');
                return out;
            }
            out.push(ch);
            count += 1;
        }
    }
    out
}

pub(super) fn load_file(path: &Path) -> Result<SessionData, Error> {
    let mut lines = Lines::open(path)?;
    let (mut meta, mut last_valid_end) = lines.header()?;

    let mut messages = Vec::new();
    let mut usage = TokenUsage::default();
    let mut context = 0;
    let mut corrupt = None;

    while let Some((line, line_end)) = lines.next_line()? {
        // Only a half-written last line, left by a crash, is recovered.
        if let Some(err) = corrupt {
            return Err(err);
        }
        match serde_json::from_str::<SessionEntry>(line.trim()) {
            Ok(SessionEntry::Message { message }) => {
                messages.push(message);
                last_valid_end = line_end;
            }
            Ok(SessionEntry::Compact {
                messages: compacted,
            }) => {
                messages = compacted;
                context = 0;
                last_valid_end = line_end;
            }
            Ok(SessionEntry::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
                context_tokens,
            }) => {
                usage += TokenUsage {
                    input: input_tokens,
                    output: output_tokens,
                    cached: cached_tokens,
                };
                if context_tokens > 0 {
                    context = context_tokens;
                }
                last_valid_end = line_end;
            }
            Ok(SessionEntry::Model { model: next }) => {
                meta.model = next;
                last_valid_end = line_end;
            }
            Ok(SessionEntry::Header { .. }) => {
                last_valid_end = line_end;
            }
            Err(e) => {
                let at = format!("{}:{}", path.display(), lines.count);
                corrupt = Some(format!("{at}: corrupt session entry: {e}").into());
            }
        }
    }

    let repair_len = (corrupt.is_some() || !lines.newline).then_some(last_valid_end as u64);

    Ok(SessionData {
        meta,
        messages,
        usage,
        context,
        repair_len,
    })
}
