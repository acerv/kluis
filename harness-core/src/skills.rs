// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Skill discovery: folders holding a `SKILL.md` whose front matter names
//! and describes the skill. Only the name, description and path go into the
//! system prompt; the model reads the rest when a task needs it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::front_matter::{clean_description, split_front_matter, valid_name, NAME_RULE};
use crate::prompt::read_capped;

/// File describing a skill, in its folder.
pub const SKILL_FILE: &str = "SKILL.md";

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
    let (front, _) = split_front_matter(text)?.ok_or("no front matter")?;
    let name = front.name.filter(|n| valid_name(n)).ok_or(NAME_RULE)?;
    let description = clean_description(front.description.as_deref().unwrap_or_default());
    if description.is_empty() {
        return Err("no description");
    }
    Ok((name, description))
}
