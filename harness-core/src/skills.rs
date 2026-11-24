// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Skill discovery: folders holding a `SKILL.md` whose front matter names
//! and describes the skill. Only the name, description and path go into the
//! system prompt; the model reads the rest when a task needs it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::prompt::read_capped;

/// File describing a skill, in its folder.
pub const SKILL_FILE: &str = "SKILL.md";

/// Longest skill name.
const MAX_NAME_CHARS: usize = 64;

/// Longest description kept, in characters.
const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Bytes read from the start of a `SKILL.md` to find its front matter.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Most skill folders read.
pub const MAX_SKILLS: usize = 100;

/// A skill the model can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Its `SKILL.md`.
    pub path: PathBuf,
}

/// Skills found, sorted by name, and warnings about the ones that could
/// not be used.
#[derive(Debug, Default)]
pub struct Skills {
    pub skills: Vec<Skill>,
    pub warnings: Vec<String>,
}

/// Discover the skills in the folders of `dir` (e.g. `~/.kluis/skills`).
///
/// Only skills the user put there are trusted: neither `dir`, a skill
/// folder nor its `SKILL.md` may be a symlink, which could lead elsewhere.
pub fn find_skills(dir: &Path) -> Skills {
    let mut found = BTreeMap::new();
    let mut warnings = Vec::new();
    let real = std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir());
    let Some(entries) = std::fs::read_dir(dir).ok().filter(|_| real) else {
        return Skills::default();
    };
    // Bounded, as anything could create any number of folders there.
    let mut dirs: Vec<_> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .take(MAX_SKILLS + 1)
        .collect();
    if dirs.len() > MAX_SKILLS {
        dirs.pop();
        warnings.push(format!(
            "{}: only {MAX_SKILLS} skill folders are read",
            dir.display()
        ));
    }
    dirs.sort();
    for skill_dir in dirs {
        let path = skill_dir.join(SKILL_FILE);
        let Some((bytes, _)) = read_capped(&path, MAX_HEADER_BYTES) else {
            continue;
        };
        let (name, description) = match parse_skill(&String::from_utf8_lossy(&bytes)) {
            Ok(skill) => skill,
            Err(err) => {
                warnings.push(format!("skill {}: {err}", path.display()));
                continue;
            }
        };
        if found.contains_key(&name) {
            warnings.push(format!(
                "skill {}: another skill is named {name}",
                path.display()
            ));
            continue;
        }
        let skill = Skill {
            name: name.clone(),
            description,
            path,
        };
        found.insert(name, skill);
    }
    Skills {
        skills: found.into_values().collect(),
        warnings,
    }
}

/// The name and description in the front matter of a `SKILL.md`.
fn parse_skill(text: &str) -> Result<(String, String), &'static str> {
    let mut lines = text.strip_prefix('\u{feff}').unwrap_or(text).lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Err("no front matter");
    }
    let (mut name, mut description) = (None, None);
    // The value that indented lines continue.
    let mut current: Option<&mut Option<String>> = None;
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
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
        let value = value.trim();
        let value = match value {
            // A block scalar: the text is on the indented lines below.
            ">" | ">-" | "|" | "|-" => "",
            _ => unquote(value),
        };
        let slot = match key.trim() {
            "name" => &mut name,
            "description" => &mut description,
            _ => {
                current = None;
                continue;
            }
        };
        *slot = Some(value.to_owned());
        current = Some(slot);
    }
    if !closed {
        return Err("unterminated front matter");
    }
    let name = name.filter(|n| valid_name(n)).ok_or(
        "the name must be 1 to 64 lowercase letters, digits or hyphens, \
         not starting or ending with a hyphen",
    )?;
    // Without control characters, which could drive the terminal showing
    // it.
    let mut description: String = description
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if description.is_empty() {
        return Err("no description");
    }
    if let Some((end, _)) = description.char_indices().nth(MAX_DESCRIPTION_CHARS) {
        description.truncate(end);
        description.push('…');
    }
    Ok((name, description))
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

fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME_CHARS).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
