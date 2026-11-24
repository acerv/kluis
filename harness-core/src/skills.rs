// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Skill discovery: folders holding a `SKILL.md` whose front matter names
//! and describes the skill. Only the name, description and path go into the
//! system prompt; the model reads the rest when a task needs it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::prompt::{project_dirs, read_capped};

/// Folder holding the skills, below a project folder or the home folder.
pub const SKILLS_DIR: &str = ".agents/skills";

/// File describing a skill, in its folder.
pub const SKILL_FILE: &str = "SKILL.md";

/// Longest skill name.
const MAX_NAME_CHARS: usize = 64;

/// Longest description kept, in characters.
const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Bytes read from the start of a `SKILL.md` to find its front matter.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Most skills listed to the model.
pub const MAX_SKILLS: usize = 100;

/// A skill the model can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Its `SKILL.md`.
    pub path: PathBuf,
}

/// Skills found for `cwd`, sorted by name, and warnings about the ones
/// that could not be used.
#[derive(Debug, Default)]
pub struct Skills {
    pub skills: Vec<Skill>,
    pub warnings: Vec<String>,
}

/// Discover the skills in the `global` folders (e.g. `~/.agents/skills`),
/// then in the [`SKILLS_DIR`] of every folder from the git repository root
/// down to `cwd`. A skill found later replaces one of the same name.
///
/// Neither a skill folder nor its `SKILL.md` may be a symlink: the
/// sandboxed model can write the project folders, and could link them to
/// files the sandbox hides for the host to read. Nor may anything on the
/// way from a project folder to its [`SKILLS_DIR`]. A `global` folder,
/// which only the user writes, may be one.
pub fn find_skills(cwd: &Path, global: &[PathBuf]) -> Skills {
    let mut found = BTreeMap::new();
    let mut warnings = Vec::new();
    // Each folder to look in, with its canonical path to skip it when seen.
    let mut roots: Vec<(PathBuf, PathBuf)> = Vec::new();
    let global = global
        .iter()
        .filter_map(|dir| Some((dir.clone(), std::fs::canonicalize(dir).ok()?)));
    let project = project_dirs(cwd).into_iter().filter_map(|dir| {
        let root = dir.join(SKILLS_DIR);
        // Only a real folder, with no symlink on the way from `dir`.
        let canon = std::fs::canonicalize(dir).ok()?.join(SKILLS_DIR);
        (std::fs::canonicalize(&root).ok()? == canon).then_some((root, canon))
    });
    for (root, canon) in global.chain(project) {
        if !roots.iter().any(|(_, seen)| *seen == canon) {
            roots.push((root, canon));
        }
    }
    for (root, _) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        // Bounded: the model can create any number of folders.
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
                root.display()
            ));
        }
        dirs.sort();
        for dir in dirs {
            let path = dir.join(SKILL_FILE);
            let Some((bytes, _)) = read_capped(&path, MAX_HEADER_BYTES) else {
                continue;
            };
            match parse_skill(&String::from_utf8_lossy(&bytes)) {
                Ok((name, description)) => {
                    found.insert(
                        name.clone(),
                        Skill {
                            name,
                            description,
                            path,
                        },
                    );
                }
                Err(err) => warnings.push(format!("skill {}: {err}", path.display())),
            }
        }
    }
    if found.len() > MAX_SKILLS {
        warnings.push(format!(
            "{} skills found, only the first {MAX_SKILLS} by name are used",
            found.len()
        ));
    }
    Skills {
        skills: found.into_values().take(MAX_SKILLS).collect(),
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
