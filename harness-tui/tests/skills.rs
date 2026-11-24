// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tests of `kl --install-skills`.

mod common;

use std::fs;
use std::process::Output;

use common::{kl, stderr, stdout, tempdir};
use harness_core::BUNDLED_SKILLS;

/// What `--install-skills` prints when it did `what` to the bundled
/// skills after the first `skip` ones, `{}` standing for the name.
fn lines(skip: usize, what: &str) -> String {
    BUNDLED_SKILLS[skip..]
        .iter()
        .map(|&(name, _)| what.replace("{}", name) + "\n")
        .collect()
}

fn install(home: &std::path::Path) -> Output {
    kl(home).arg("--install-skills").output().unwrap()
}

#[test]
fn installs_then_keeps_edited_skills() {
    let home = tempdir();
    let out = install(home.path());
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), lines(0, "Added {}."));

    let (first, _) = BUNDLED_SKILLS[0];
    let edited = home
        .path()
        .join(".kluis/skills")
        .join(first)
        .join("SKILL.md");
    fs::write(&edited, "mine\n").unwrap();
    let out = install(home.path());
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), lines(1, "{} is up to date."));
    assert!(
        stderr(&out).contains(&format!("warning: kept {first}")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(&edited).unwrap(), "mine\n");
    assert!(edited.with_file_name("SKILL.md.new").is_file());
}

#[test]
fn fails_on_a_symlinked_skill() {
    let home = tempdir();
    let skills = home.path().join(".kluis/skills");
    fs::create_dir_all(&skills).unwrap();
    let (first, _) = BUNDLED_SKILLS[0];
    std::os::unix::fs::symlink(home.path(), skills.join(first)).unwrap();

    let out = install(home.path());
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).contains(&format!("error: {first}:")),
        "{out:?}"
    );
    assert_eq!(stdout(&out), lines(1, "Added {}."));
}
