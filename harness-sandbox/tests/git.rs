// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Git on the host runs commands a repository's config names: a repository
//! created in a writable folder must not stay usable.

use std::path::{Path, PathBuf};

use harness_sandbox::{GitGuard, Sandbox};

/// A repository at `dir`, with a config that would run a command.
fn repository(dir: &Path) -> PathBuf {
    let git = dir.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("config"), "[core]\n\tfsmonitor = touch pwned\n").unwrap();
    git
}

/// Where the guard moved each new repository.
fn disable_new(guard: &mut GitGuard) -> Vec<(PathBuf, PathBuf)> {
    let check = guard.disable_new();
    assert!(check.unchecked.is_none(), "{:?}", check.unchecked);
    check
        .new
        .into_iter()
        .map(|repo| (repo.path, repo.moved_to.unwrap()))
        .collect()
}

#[test]
fn new_repository_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(dir.path())).unwrap();

    let git = repository(&dir.path().join("sub"));
    let disabled = dir.path().join("sub/.git.kl-disabled");
    assert_eq!(disable_new(&mut guard), [(git.clone(), disabled.clone())]);
    assert!(!git.exists());
    // Kept, for the user to look at.
    assert!(disabled.join("config").exists());
    assert!(disable_new(&mut guard).is_empty());
}

#[test]
fn repository_in_the_writable_folder_itself_is_disabled() {
    // As `git init` in a project that was not a repository.
    let dir = tempfile::tempdir().unwrap();
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(dir.path())).unwrap();

    let git = repository(dir.path());
    assert_eq!(disable_new(&mut guard).len(), 1);
    assert!(!git.exists());
}

#[test]
fn existing_repositories_stay() {
    let dir = tempfile::tempdir().unwrap();
    let root = repository(dir.path());
    let nested = repository(&dir.path().join("vendor/lib"));
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(dir.path())).unwrap();

    assert!(disable_new(&mut guard).is_empty());
    assert!(root.exists() && nested.exists());
}

#[test]
fn git_file_is_disabled() {
    // A `.git` file sends git to a repository anywhere.
    let dir = tempfile::tempdir().unwrap();
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(dir.path())).unwrap();

    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join(".git"), "gitdir: ../elsewhere\n").unwrap();
    assert_eq!(disable_new(&mut guard).len(), 1);
    assert!(sub.join(".git.kl-disabled").is_file());
}

#[test]
fn taken_name_gets_a_number() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git.kl-disabled")).unwrap();
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(dir.path())).unwrap();

    repository(dir.path());
    let moved = disable_new(&mut guard);
    assert_eq!(moved[0].1, dir.path().join(".git.kl-disabled-2"));
}

#[test]
fn read_only_and_denied_folders_are_left_alone() {
    // Commands cannot write there.
    let dir = tempfile::tempdir().unwrap();
    let (read, denied) = (dir.path().join("read"), dir.path().join("work/secret"));
    std::fs::create_dir_all(&read).unwrap();
    std::fs::create_dir_all(&denied).unwrap();
    let sandbox = Sandbox::new()
        .allow_read(&read)
        .allow_write(dir.path().join("work"))
        .deny_read(&denied);
    let mut guard = GitGuard::new(&sandbox).unwrap();

    let in_read = repository(&read);
    let in_denied = repository(&denied);
    assert!(disable_new(&mut guard).is_empty());
    assert!(in_read.exists() && in_denied.exists());
}

/// Set the permissions of `dir` to `mode`.
fn chmod(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn unreadable_folder_stops_the_guard() {
    // A repository there could not be told apart from a new one.
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    chmod(&sub, 0o300);
    let res = GitGuard::new(&Sandbox::new().allow_write(dir.path()));
    chmod(&sub, 0o700);
    let err = res.unwrap_err();
    assert!(
        err.message.starts_with(&format!("{}:", sub.display())),
        "{err}"
    );
}

#[test]
fn repository_in_an_unreadable_folder_is_reported() {
    // A command can still use it by name.
    let dir = tempfile::tempdir().unwrap();
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(dir.path())).unwrap();

    let sub = dir.path().join("sub");
    repository(&sub);
    chmod(&sub, 0o300);
    let check = guard.disable_new();
    chmod(&sub, 0o700);
    assert!(check.new.is_empty());
    let err = check.unchecked.unwrap();
    assert!(err.message.contains(&*sub.to_string_lossy()), "{err}");
}

#[test]
fn repository_put_in_place_of_a_link_is_disabled() {
    // A link in the writable folder itself cannot be kept read-only.
    let dir = tempfile::tempdir().unwrap();
    let (project, real) = (dir.path().join("project"), dir.path().join("real.git"));
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&real).unwrap();
    let git = project.join(".git");
    std::os::unix::fs::symlink(&real, &git).unwrap();
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(&project)).unwrap();
    assert!(disable_new(&mut guard).is_empty());

    std::fs::remove_file(&git).unwrap();
    repository(&project);
    let disabled = project.join(".git.kl-disabled");
    assert_eq!(disable_new(&mut guard), [(git.clone(), disabled.clone())]);
    assert!(disabled.join("config").exists());

    // So is another link.
    std::os::unix::fs::symlink(&disabled, &git).unwrap();
    let again = project.join(".git.kl-disabled-2");
    assert_eq!(disable_new(&mut guard), [(git, again)]);
}
