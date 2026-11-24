// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use harness_core::{find_skills, system_prompt, Skill, MAX_SKILLS, SKILLS_DIR, SKILL_FILE};

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
fn finds_global_and_project_skills_sorted_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("home/.agents/skills");
    let cwd = tmp.path().join("project");
    let path = skill(&global, "review", &front("review", "Review a patch"));
    skill(&cwd.join(SKILLS_DIR), "build", &front("build", "Build it"));

    let found = find_skills(&cwd, Some(&global));
    assert_eq!(names(&found.skills), ["build", "review"]);
    assert_eq!(found.skills[1].description, "Review a patch");
    assert_eq!(found.skills[1].path, path);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn more_specific_skill_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let cwd = repo.join("sub");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let global = tmp.path().join("global");
    skill(&global, "a", &front("test", "global"));
    skill(&repo.join(SKILLS_DIR), "a", &front("test", "repo"));
    let path = skill(&cwd.join(SKILLS_DIR), "a", &front("test", "folder"));

    let found = find_skills(&cwd, Some(&global));
    assert_eq!(found.skills.len(), 1);
    assert_eq!(found.skills[0].description, "folder");
    assert_eq!(found.skills[0].path, path);
}

#[test]
fn parses_quotes_and_folded_descriptions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(SKILLS_DIR);
    skill(
        &root,
        "a",
        "---\nname: \"quoted\"\ndescription: 'Say: hi'\n---\n",
    );
    skill(
        &root,
        "b",
        "\u{feff}---\nname: folded\nlicense: MIT\ndescription: >-\n  First line\n  second line\nmetadata:\n  key: value\n---\n",
    );
    skill(&root, "c", &front("long", &"x".repeat(2000)));
    skill(&root, "d", &front("escape", "a\u{1b}]52;c;eA==\u{7}b"));

    let found = find_skills(tmp.path(), None);
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
    let root = tmp.path().join(SKILLS_DIR);
    skill(&root, "a", "# no front matter\n");
    skill(&root, "b", "---\nname: open\ndescription: never closed\n");
    skill(&root, "c", &front("Bad_Name", "upper case"));
    skill(&root, "d", &front("-dash", "leading hyphen"));
    skill(&root, "e", "---\nname: nodesc\n---\n");
    skill(&root, "f", &front("good", "fine"));
    // A folder without a SKILL.md is not a skill.
    fs::create_dir_all(root.join("g")).unwrap();

    let found = find_skills(tmp.path(), None);
    assert_eq!(names(&found.skills), ["good"]);
    assert_eq!(found.warnings.len(), 5, "{:?}", found.warnings);
    assert!(found.warnings[0].contains("no front matter"));
    assert!(found.warnings[1].contains("unterminated"));
    assert!(found.warnings[4].contains("no description"));
}

#[test]
fn caps_the_number_of_skills() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(SKILLS_DIR);
    for i in 0..=MAX_SKILLS {
        let name = format!("s{i:03}");
        skill(&root, &name, &front(&name, "d"));
    }
    let found = find_skills(tmp.path(), None);
    assert_eq!(found.skills.len(), MAX_SKILLS);
    assert_eq!(found.warnings.len(), 1);
}

#[test]
fn symlinks_in_project_skills_are_ignored() {
    // The sandboxed model can write the project folder, and could link a
    // skill to files the sandbox hides, for the host to read.
    let tmp = tempfile::tempdir().unwrap();
    let hidden = tmp.path().join("hidden");
    let secret = skill(&hidden, "secret", &front("secret", "sk-secret"));
    let cwd = tmp.path().join("project");
    let root = cwd.join(SKILLS_DIR);
    fs::create_dir_all(root.join("file")).unwrap();
    symlink(&secret, root.join("file").join(SKILL_FILE)).unwrap();
    symlink(hidden.join("secret"), root.join("dir")).unwrap();

    assert!(find_skills(&cwd, None).skills.is_empty());

    // Nor may the skills folder itself lead elsewhere.
    let other = tmp.path().join("other");
    fs::create_dir_all(other.join(".agents")).unwrap();
    symlink(&hidden, other.join(SKILLS_DIR)).unwrap();
    assert!(find_skills(&other, None).skills.is_empty());
}

#[test]
fn symlinked_global_skills_folder_is_followed() {
    // Only the user writes the global folder.
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    skill(&real, "a", &front("mine", "d"));
    let global = tmp.path().join("skills");
    symlink(&real, &global).unwrap();
    let cwd = tmp.path().join("project");
    fs::create_dir_all(&cwd).unwrap();

    let found = find_skills(&cwd, Some(&global));
    assert_eq!(names(&found.skills), ["mine"]);
    // Shown where the sandbox shows it, not where the link leads.
    assert_eq!(found.skills[0].path, global.join("a").join(SKILL_FILE));
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
