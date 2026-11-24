// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What skills and commands share: the listing of their folder and
//! their YAML front matter, of which only the `name` and `description`
//! keys are read, as plain or folded strings.

use std::fs::DirEntry;
use std::path::{Path, PathBuf};

/// Longest name of a skill or command.
const MAX_NAME_CHARS: usize = 64;

/// Longest description kept, in characters.
const MAX_DESCRIPTION_CHARS: usize = 1024;

/// The keys read from a front matter.
#[derive(Debug, Default)]
pub(crate) struct FrontMatter {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
}

/// The entries of `dir` that `keep` takes, sorted. At most `max` are
/// listed, as anything could create any number of them there: `warnings`
/// then tells that only `max` `what` are read.
///
/// Lists nothing when `dir` is a symlink, which could lead elsewhere, or
/// can't be read.
pub(crate) fn list_dir(
    dir: &Path,
    max: usize,
    what: &str,
    warnings: &mut Vec<String>,
    keep: impl Fn(&DirEntry) -> bool,
) -> Vec<PathBuf> {
    let real = std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir());
    let Some(entries) = std::fs::read_dir(dir).ok().filter(|_| real) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .filter(keep)
        .map(|entry| entry.path())
        .take(max + 1)
        .collect();
    if paths.len() > max {
        paths.pop();
        warnings.push(format!("{}: only {max} {what} are read", dir.display()));
    }
    paths.sort();
    paths
}

/// The front matter `text` starts with and the text after it, or `None`
/// when it has none.
pub(crate) fn split_front_matter(text: &str) -> Result<Option<(FrontMatter, &str)>, &'static str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next().filter(|line| line.trim_end() == "---") else {
        return Ok(None);
    };
    let mut end = first.len();
    let mut front = FrontMatter::default();
    // The value that indented lines continue.
    let mut current: Option<&mut Option<String>> = None;
    for line in lines {
        end += line.len();
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim_end() == "---" {
            return Ok(Some((front, &text[end..])));
        }
        if line.starts_with([' ', '\t']) {
            if let Some(Some(value)) = current.as_deref_mut() {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            current = None;
            continue;
        };
        let value = match value.trim() {
            // A block scalar: the text is on the indented lines below.
            ">" | ">-" | "|" | "|-" => "",
            value => unquote(value),
        };
        let slot = match key.trim() {
            "name" => &mut front.name,
            "description" => &mut front.description,
            _ => {
                current = None;
                continue;
            }
        };
        *slot = Some(value.to_owned());
        current = Some(slot);
    }
    Err("unterminated front matter")
}

/// `description` without control characters, which could drive the
/// terminal showing it, and cut to its longest length.
pub(crate) fn clean_description(description: &str) -> String {
    let mut clean: String = description
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if let Some((end, _)) = clean.char_indices().nth(MAX_DESCRIPTION_CHARS) {
        clean.truncate(end);
        clean.push('…');
    }
    clean
}

/// Why a name is refused.
pub(crate) const NAME_RULE: &str = "the name must be 1 to 64 lowercase letters, digits or \
     hyphens, not starting or ending with a hyphen";

/// Whether `name` may name a skill or command.
pub(crate) fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME_CHARS).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}
