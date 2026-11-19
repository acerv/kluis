// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::borrow::Cow;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::path::resolve;
use crate::write::atomic_write;

const BOM: &str = "\u{feff}";

/// A call of the `edit` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditTool {
    pub path: String,
    /// Replacements, all matched against the original file.
    pub edits: Vec<Edit>,
}

/// One exact-text replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditOutput {
    pub replacements: usize,
}

/// A matched edit, in line-feed-normalized offsets.
struct Match<'a> {
    index: usize,
    start: usize,
    end: usize,
    new_text: &'a str,
}

impl EditTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string" },
                            "newText": { "type": "string" }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    /// The `edit` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new("edit")
            .with_description(
                "Edit a file by replacing exact text. Each oldText must match \
                 exactly one place in the file.",
            )
            .with_schema(Self::schema())
    }

    /// Apply the edits to an existing text file.
    ///
    /// A leading `~` in the path expands to `$HOME`; a relative path starts
    /// at the current directory. Text is matched with `\r\n` and `\r` read
    /// as `\n`; lines keep their original endings and a leading byte order
    /// mark is preserved. Nothing is written unless every edit matches.
    pub async fn run(self) -> Result<EditOutput, ToolError> {
        self.run_in(Path::new(".")).await
    }

    /// Like [`run`](Self::run), with relative paths starting at `cwd`.
    pub async fn run_in(self, cwd: &Path) -> Result<EditOutput, ToolError> {
        let path = resolve(cwd, &self.path);
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::io(&self.path, e))?;
        if meta.is_dir() {
            return Err(ToolError::new(format!("{}: is a directory", self.path)));
        }
        // FIFOs, sockets and devices can block forever or have side effects.
        if !meta.is_file() {
            return Err(ToolError::new(format!("{}: not a regular file", self.path)));
        }

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::io(&self.path, e))?;
        if bytes.contains(&0) {
            return Err(ToolError::new(format!("{}: binary file", self.path)));
        }
        // Replacing text in lossily decoded content would corrupt the file.
        let raw = String::from_utf8(bytes)
            .map_err(|_| ToolError::new(format!("{}: not valid UTF-8", self.path)))?;

        let (bom, text) = match raw.strip_prefix(BOM) {
            Some(text) => (BOM, text),
            None => ("", raw.as_str()),
        };
        let edited = apply(&self.path, bom, text, &self.edits)?;

        let replacements = self.edits.len();
        atomic_write(path, edited, Some(meta.permissions().mode()))
            .await
            .map_err(|e| ToolError::io(&self.path, e))?;

        Ok(EditOutput { replacements })
    }
}

/// Apply `edits` to `text` and return the new content, starting with `bom`.
fn apply(path: &str, bom: &str, text: &str, edits: &[Edit]) -> Result<String, ToolError> {
    let err = |msg: String| ToolError::new(format!("{path}: {msg}"));
    let which = |i: usize| match edits.len() {
        1 => "the text".to_owned(),
        _ => format!("edits[{i}]"),
    };

    if edits.is_empty() {
        return Err(err("edits must contain at least one replacement".into()));
    }

    // Match on line-feed-only text; the model never writes `\r`.
    let lf = to_lf(text);
    let olds: Vec<Cow<str>> = edits.iter().map(|e| to_lf(&e.old_text)).collect();
    let news: Vec<Cow<str>> = edits.iter().map(|e| to_lf(&e.new_text)).collect();

    let mut matches = Vec::with_capacity(edits.len());
    for (index, (old, new)) in olds.iter().zip(&news).enumerate() {
        if old.is_empty() {
            return Err(err(format!(
                "oldText of {} must not be empty",
                which(index)
            )));
        }
        if old == new {
            return Err(err(format!(
                "{} makes no change; oldText and newText are identical",
                which(index)
            )));
        }
        let Some(start) = lf.find(old.as_ref()) else {
            return Err(err(format!(
                "could not find {}. The old text must match exactly, including \
                 all whitespace and newlines",
                which(index)
            )));
        };
        let occurrences = count_overlapping(&lf, old, start);
        if occurrences > 1 {
            return Err(err(format!(
                "found {occurrences} occurrences of {}. The text must be unique; \
                 add more context to make it unique",
                which(index)
            )));
        }
        matches.push(Match {
            index,
            start,
            end: start + old.len(),
            new_text: new,
        });
    }

    matches.sort_by_key(|m| m.start);
    for pair in matches.windows(2) {
        if pair[0].end > pair[1].start {
            return Err(err(format!(
                "edits[{}] and edits[{}] overlap. Merge them into one edit or \
                 target separate regions",
                pair[0].index, pair[1].index
            )));
        }
    }

    Ok(splice(bom, text, &matches))
}

/// `s` with `\r\n` and lone `\r` turned into `\n`, copied only if needed.
fn to_lf(s: &str) -> Cow<'_, str> {
    if s.contains('\r') {
        Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(s)
    }
}

/// Occurrences of `needle` in `haystack`, overlapping ones included,
/// given the first one at `first`.
fn count_overlapping(haystack: &str, needle: &str, first: usize) -> usize {
    let mut count = 0;
    let mut at = Some(first);
    while let Some(i) = at {
        count += 1;
        // Restart one character later, so overlapping matches count too.
        let next = i + haystack[i..].chars().next().map_or(1, char::len_utf8);
        at = haystack[next..].find(needle).map(|j| next + j);
    }
    count
}

/// Apply matches found in the line-feed-normalized text to the original
/// text, so untouched lines keep their endings and replacements use the
/// ending of the line they land on.
fn splice(bom: &str, text: &str, matches: &[Match<'_>]) -> String {
    // Only `\r\n` changes length when normalized: collect the normalized
    // offset of each, to map offsets back to `text`.
    let crlf: Vec<usize> = text
        .match_indices("\r\n")
        .enumerate()
        .map(|(n, (i, _))| i - n)
        .collect();
    let original = |offset: usize| offset + crlf.partition_point(|&p| p < offset);

    let mut out = String::with_capacity(bom.len() + text.len());
    out.push_str(bom);
    let mut pos = 0;
    for m in matches {
        let (start, end) = (original(m.start), original(m.end));
        out.push_str(&text[pos..start]);
        match line_ending(text, start) {
            "\n" => out.push_str(m.new_text),
            ending => out.push_str(&m.new_text.replace('\n', ending)),
        }
        pos = end;
    }
    out.push_str(&text[pos..]);
    out
}

/// Ending of the line at `offset`; the last line, which may have none,
/// follows the line before it.
fn line_ending(text: &str, offset: usize) -> &'static str {
    let is_break = |c: char| c == '\n' || c == '\r';
    let at = text[offset..]
        .find(is_break)
        .map(|i| offset + i)
        .or_else(|| text[..offset].rfind(is_break));
    let bytes = text.as_bytes();
    match at {
        Some(i) if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') => "\r\n",
        Some(i) if bytes[i] == b'\n' && i > 0 && bytes[i - 1] == b'\r' => "\r\n",
        Some(i) if bytes[i] == b'\r' => "\r",
        _ => "\n",
    }
}
