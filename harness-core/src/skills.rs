// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Skills: folders holding a `SKILL.md` whose front matter names and
//! describes the skill. Only the names and descriptions go into the system
//! prompt; the model loads a skill with the `skill` tool when a task needs
//! it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::front_matter::{clean_description, list_dir, split_front_matter, valid_name, NAME_RULE};
use crate::private_file::read_capped;
use crate::tools::ToolError;

/// Name of the tool loading a skill.
pub const SKILL_TOOL: &str = "skill";

/// Maximum bytes of a `SKILL.md` the tool returns.
pub const MAX_SKILL_BYTES: usize = 64 * 1024;

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
    let dirs = list_dir(dir, MAX_SKILLS, "skill folders", &mut warnings, |entry| {
        entry.file_type().is_ok_and(|t| t.is_dir())
    });
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

/// A call of the `skill` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillTool {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillOutput {
    /// The folder of the skill, which paths in it start from.
    pub folder: PathBuf,
    /// Its `SKILL.md`.
    pub instructions: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl SkillTool {
    /// The `skill` tool, as sent to the model, taking one of `skills`.
    pub fn definition(skills: &[Skill]) -> Tool {
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        let schema: Value = json!({
            "type": "object",
            "properties": { "name": { "type": "string", "enum": names } },
            "required": ["name"]
        });
        Tool::new(SKILL_TOOL)
            .with_description(
                "Load the instructions of a skill from the system prompt. Use it \
                 before a task the skill is for, then follow them.",
            )
            .with_schema(schema)
    }

    /// The `SKILL.md` of the skill named in the call, among `skills`.
    pub fn run(self, skills: &[Skill]) -> Result<SkillOutput, ToolError> {
        let skill = skills
            .iter()
            .find(|s| s.name == self.name)
            .ok_or_else(|| ToolError::new(format!("no skill named {}", self.name)))?;
        let (bytes, truncated) = read_capped(&skill.path, MAX_SKILL_BYTES)
            .ok_or_else(|| ToolError::new(format!("cannot read {}", skill.path.display())))?;
        Ok(SkillOutput {
            folder: skill.path.parent().unwrap_or(&skill.path).to_path_buf(),
            instructions: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        })
    }
}
