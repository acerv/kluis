// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tests of `kl --install-skills`.

mod common;

use std::fs;
use std::process::Output;

use common::{kl, stderr, stdout, tempdir};

fn install(home: &std::path::Path) -> Output {
    kl(home).arg("--install-skills").output().unwrap()
}

#[test]
fn installs_then_keeps_edited_skills() {
    let home = tempdir();
    let out = install(home.path());
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), "Added mentor.\nAdded study.\n");

    let mentor = home.path().join(".kluis/skills/mentor/SKILL.md");
    fs::write(&mentor, "mine\n").unwrap();
    let out = install(home.path());
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), "study is up to date.\n");
    assert!(stderr(&out).contains("warning: kept mentor"), "{out:?}");
    assert_eq!(fs::read_to_string(&mentor).unwrap(), "mine\n");
    assert!(mentor.with_file_name("SKILL.md.new").is_file());
}

#[test]
fn fails_on_a_symlinked_skill() {
    let home = tempdir();
    let skills = home.path().join(".kluis/skills");
    fs::create_dir_all(&skills).unwrap();
    std::os::unix::fs::symlink(home.path(), skills.join("mentor")).unwrap();

    let out = install(home.path());
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr(&out).contains("error: mentor:"), "{out:?}");
    assert_eq!(stdout(&out), "Added study.\n");
}
