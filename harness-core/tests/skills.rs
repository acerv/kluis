// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use harness_core::{find_skills, system_prompt, Skill, MAX_SKILLS, SKILL_FILE};

/// Write the skill `dir` in the skills folder `root` with `text` as its
/// `SKILL.md`, returning the path of the file.
fn skill(root: &Path, dir: &str, text: &str) -> PathBuf {
    let dir = root.join(dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(SKILL_FILE);
    fs::write(&path, text).unwrap();
    path
}

fn front(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n# Body\n")
}

fn names(skills: &[Skill]) -> Vec<&str> {
    skills.iter().map(|s| s.name.as_str()).collect()
}

#[test]
fn finds_skills_sorted_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let path = skill(&root, "z", &front("review", "Review a patch"));
    skill(&root, "a", &front("build", "Build it"));

    let found = find_skills(&root);
    assert_eq!(names(&found.skills), ["build", "review"]);
    assert_eq!(found.skills[1].description, "Review a patch");
    assert_eq!(found.skills[1].path, path);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn missing_folder_has_no_skills() {
    let tmp = tempfile::tempdir().unwrap();
    let found = find_skills(&tmp.path().join("missing"));
    assert!(found.skills.is_empty() && found.warnings.is_empty());
}

#[test]
fn parses_quotes_and_folded_descriptions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    skill(
        root,
        "a",
        "---\nname: \"quoted\"\ndescription: 'Say: hi'\n---\n",
    );
    skill(
        root,
        "b",
        "\u{feff}---\nname: folded\nlicense: MIT\ndescription: >-\n  First line\n  second line\nmetadata:\n  key: value\n---\n",
    );
    skill(root, "c", &front("long", &"x".repeat(2000)));
    skill(root, "d", &front("escape", "a\u{1b}]52;c;eA==\u{7}b"));

    let found = find_skills(root);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    let get = |name| {
        let skill = found.skills.iter().find(|s| s.name == name).unwrap();
        skill.description.clone()
    };
    assert_eq!(get("quoted"), "Say: hi");
    assert_eq!(get("folded"), "First line second line");
    assert_eq!(get("long").chars().count(), 1025);
    assert_eq!(get("escape"), "a]52;c;eA==b");
}

#[test]
fn warns_about_invalid_skills() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    skill(root, "a", "# no front matter\n");
    skill(root, "b", "---\nname: open\ndescription: never closed\n");
    skill(root, "c", &front("Bad_Name", "upper case"));
    skill(root, "d", &front("-dash", "leading hyphen"));
    skill(root, "e", "---\nname: nodesc\n---\n");
    skill(root, "f", &front("good", "fine"));
    skill(root, "g", &front("good", "again"));
    // A folder without a SKILL.md is not a skill.
    fs::create_dir_all(root.join("h")).unwrap();

    let found = find_skills(root);
    assert_eq!(names(&found.skills), ["good"]);
    assert_eq!(found.skills[0].description, "fine");
    assert_eq!(found.warnings.len(), 6, "{:?}", found.warnings);
    assert!(found.warnings[0].contains("no front matter"));
    assert!(found.warnings[1].contains("unterminated"));
    assert!(found.warnings[4].contains("no description"));
    assert!(found.warnings[5].contains("another skill is named good"));
}

#[test]
fn caps_the_number_of_skills() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..=MAX_SKILLS {
        let name = format!("s{i:03}");
        skill(tmp.path(), &name, &front(&name, "d"));
    }
    let found = find_skills(tmp.path());
    assert_eq!(found.skills.len(), MAX_SKILLS);
    assert_eq!(found.warnings.len(), 1);
}

#[test]
fn symlinks_are_ignored() {
    // Only skills put in the folder itself are trusted.
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    let file = skill(&outside, "linked", &front("linked", "from elsewhere"));
    let root = tmp.path().join("skills");
    fs::create_dir_all(root.join("file")).unwrap();
    symlink(&file, root.join("file").join(SKILL_FILE)).unwrap();
    symlink(outside.join("linked"), root.join("dir")).unwrap();
    assert!(find_skills(&root).skills.is_empty());

    // Nor may the skills folder itself lead elsewhere.
    let linked_root = tmp.path().join("linked-skills");
    symlink(&outside, &linked_root).unwrap();
    assert!(find_skills(&linked_root).skills.is_empty());
}

#[test]
fn system_prompt_lists_skills() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = [Skill {
        name: "review".to_owned(),
        description: "Review a patch".to_owned(),
        path: PathBuf::from("/skills/review/SKILL.md"),
    }];
    let text = system_prompt(tmp.path(), &[], &skills);
    assert!(text.contains("## Skills"), "{text}");
    assert!(
        text.contains("- review: Review a patch (/skills/review/SKILL.md)"),
        "{text}"
    );
    assert!(!system_prompt(tmp.path(), &[], &[]).contains("## Skills"));
}
