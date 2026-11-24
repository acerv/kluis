// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs;
use std::hash::Hasher;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use fnv::FnvHasher;
use harness_core::{
    find_skills, install_skills, outdated_skills, Installed, BUNDLED_SKILLS, SKILL_FILE,
};

/// What `install_skills` did with each skill in `dir`.
fn install(dir: &Path) -> Vec<(&'static str, Installed)> {
    install_skills(dir)
        .unwrap()
        .into_iter()
        .map(|(name, res)| (name, res.unwrap()))
        .collect()
}

fn all(what: Installed) -> Vec<(&'static str, Installed)> {
    BUNDLED_SKILLS
        .iter()
        .map(|&(name, _)| (name, what))
        .collect()
}

fn names() -> Vec<&'static str> {
    BUNDLED_SKILLS.iter().map(|&(name, _)| name).collect()
}

fn skill_file(dir: &Path, name: &str) -> PathBuf {
    dir.join(name).join(SKILL_FILE)
}

/// Pretend `text` was the version of `name` last installed in `dir`.
fn installed_before(dir: &Path, name: &str, text: &str) {
    fs::write(skill_file(dir, name), text).unwrap();
    let mut hasher = FnvHasher::default();
    hasher.write(text.as_bytes());
    let manifest = dir.join(".bundled");
    let old = fs::read_to_string(&manifest).unwrap();
    let kept: String = old
        .lines()
        .filter(|line| !line.ends_with(&format!(" {name}")))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(manifest, format!("{kept}{:016x} {name}\n", hasher.finish())).unwrap();
}

#[test]
fn installs_skills_that_are_found() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skills");

    assert_eq!(install(&dir), all(Installed::Added));
    let found = find_skills(&dir);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    let found: Vec<_> = found.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(found, names());
    for &(name, text) in BUNDLED_SKILLS {
        assert_eq!(fs::read_to_string(skill_file(&dir, name)).unwrap(), text);
    }

    assert_eq!(install(&dir), all(Installed::Current));
    assert!(outdated_skills(&dir).is_empty());
}

#[test]
fn readds_a_deleted_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    install(dir);
    let (name, _) = BUNDLED_SKILLS[0];
    fs::remove_dir_all(dir.join(name)).unwrap();

    assert!(outdated_skills(dir).is_empty());
    assert_eq!(install(dir)[0], (name, Installed::Added));
}

#[test]
fn updates_an_untouched_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    install(dir);
    let (name, text) = BUNDLED_SKILLS[0];
    installed_before(dir, name, "old version\n");

    assert_eq!(outdated_skills(dir), [name]);
    assert_eq!(install(dir)[0], (name, Installed::Updated));
    assert_eq!(fs::read_to_string(skill_file(dir, name)).unwrap(), text);
    assert!(outdated_skills(dir).is_empty());
}

#[test]
fn keeps_an_edited_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    install(dir);
    let (name, text) = BUNDLED_SKILLS[0];
    installed_before(dir, name, "old version\n");
    fs::write(skill_file(dir, name), "my version\n").unwrap();

    assert_eq!(install(dir)[0], (name, Installed::Kept));
    let path = skill_file(dir, name);
    assert_eq!(fs::read_to_string(&path).unwrap(), "my version\n");
    let new = path.with_file_name("SKILL.md.new");
    assert_eq!(fs::read_to_string(new).unwrap(), text);
    // The user was told; no hint until the next version.
    assert!(outdated_skills(dir).is_empty());
    assert_eq!(install(dir)[0], (name, Installed::Kept));
}

#[test]
fn edited_before_any_install_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (name, _) = BUNDLED_SKILLS[0];
    fs::create_dir(dir.join(name)).unwrap();
    fs::write(skill_file(dir, name), "mine\n").unwrap();

    assert_eq!(install(dir)[0], (name, Installed::Kept));
    assert_eq!(fs::read_to_string(skill_file(dir, name)).unwrap(), "mine\n");
}

#[test]
fn no_hint_before_any_install() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(outdated_skills(tmp.path()).is_empty());
}

#[test]
fn hints_a_skill_never_installed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(dir.join(".bundled"), "").unwrap();
    assert_eq!(outdated_skills(dir), names());
}

#[test]
fn refuses_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("elsewhere");
    fs::create_dir(&target).unwrap();

    let link = tmp.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(install_skills(&link).is_err());

    let dir = tmp.path().join("skills");
    fs::create_dir(&dir).unwrap();
    let (name, _) = BUNDLED_SKILLS[0];
    symlink(&target, dir.join(name)).unwrap();
    let done = install_skills(&dir).unwrap();
    assert!(done[0].1.is_err());
    assert!(done[1..].iter().all(|(_, res)| res.is_ok()));
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);

    // A symlinked SKILL.md is not replaced either.
    let (name, _) = BUNDLED_SKILLS[1];
    let file = skill_file(&dir, name);
    fs::remove_file(&file).unwrap();
    symlink(target.join("x"), &file).unwrap();
    let done = install_skills(&dir).unwrap();
    assert!(done[1].1.is_err());
    assert!(fs::symlink_metadata(&file).unwrap().is_symlink());
}
