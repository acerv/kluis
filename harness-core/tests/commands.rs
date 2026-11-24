// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use harness_core::{expand_command, find_commands, Command, MAX_COMMANDS, MAX_COMMAND_BYTES};

fn write(root: &Path, file: &str, text: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(file), text).unwrap();
}

fn command(template: &str) -> Command {
    Command {
        name: "review".to_owned(),
        description: String::new(),
        template: template.to_owned(),
    }
}

#[test]
fn finds_commands_sorted_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("commands");
    write(
        &root,
        "review.md",
        "---\ndescription: Review a patch\n---\n\nReview $ARGUMENTS.\n",
    );
    write(&root, "build.md", "\n# Build the project\n\nRun make.\n");
    // Not a command.
    write(&root, "notes.txt", "ignored");
    fs::create_dir_all(root.join("dir.md")).unwrap();

    let found = find_commands(&root);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    let [build, review] = found.commands.as_slice() else {
        panic!("{:?}", found.commands);
    };
    assert_eq!(build.name, "build");
    assert_eq!(build.description, "Build the project");
    assert_eq!(build.template, "# Build the project\n\nRun make.");
    assert_eq!(review.name, "review");
    assert_eq!(review.description, "Review a patch");
    assert_eq!(review.template, "Review $ARGUMENTS.");
}

#[test]
fn missing_folder_has_no_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let found = find_commands(&tmp.path().join("missing"));
    assert!(found.commands.is_empty() && found.warnings.is_empty());
}

#[test]
fn warns_about_invalid_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Upper.md", "prompt");
    write(root, "empty.md", "---\ndescription: nothing\n---\n  \n");
    write(root, "open.md", "---\ndescription: never closed\n");
    write(root, "big.md", &"x".repeat(MAX_COMMAND_BYTES + 1));
    write(root, "good.md", "fine");

    let found = find_commands(root);
    assert_eq!(found.commands.len(), 1);
    assert_eq!(found.commands[0].name, "good");
    let warnings = found.warnings.join("\n");
    assert_eq!(found.warnings.len(), 4, "{warnings}");
    for expected in [
        "Upper.md: the name",
        "empty.md: no prompt",
        "open.md: unterminated",
    ] {
        assert!(warnings.contains(expected), "{warnings}");
    }
    assert!(warnings.contains("big.md: larger than"), "{warnings}");
}

#[test]
fn strips_control_characters() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "esc.md",
        "a\u{1b}]52;c;eA==\u{7}b\r\n\tindented\n",
    );
    let found = find_commands(tmp.path());
    assert_eq!(found.commands[0].template, "a]52;c;eA==b\n\tindented");
    assert_eq!(found.commands[0].description, "a]52;c;eA==b");
}

#[test]
fn caps_the_number_of_commands() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..=MAX_COMMANDS {
        write(tmp.path(), &format!("c{i:03}.md"), "prompt");
    }
    let found = find_commands(tmp.path());
    assert_eq!(found.commands.len(), MAX_COMMANDS);
    assert_eq!(found.warnings.len(), 1);
}

#[test]
fn symlinks_are_ignored() {
    // Only commands put in the folder itself are trusted.
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    write(&outside, "linked.md", "from elsewhere");
    let root = tmp.path().join("commands");
    fs::create_dir_all(&root).unwrap();
    symlink(outside.join("linked.md"), root.join("linked.md")).unwrap();
    assert!(find_commands(&root).commands.is_empty());

    // Nor may the commands folder itself lead elsewhere.
    let linked_root = tmp.path().join("linked-commands");
    symlink(&outside, &linked_root).unwrap();
    assert!(find_commands(&linked_root).commands.is_empty());
}

#[test]
fn expands_arguments() {
    let review = command("Review $ARGUMENTS, then fix $ARGUMENTS.");
    assert_eq!(review.expand("  a.rs "), "Review a.rs, then fix a.rs.");
    assert_eq!(review.expand(""), "Review , then fix .");

    // Arguments are appended to a template without $ARGUMENTS.
    let plain = command("Review the diff.");
    assert_eq!(plain.expand(""), "Review the diff.");
    assert_eq!(plain.expand("only a.rs"), "Review the diff.\n\nonly a.rs");

    // Arguments are not expanded again.
    assert_eq!(
        review.expand("$ARGUMENTS"),
        "Review $ARGUMENTS, then fix $ARGUMENTS."
    );
}

#[test]
fn expands_only_known_commands() {
    let commands = [command("Review $ARGUMENTS.")];
    assert_eq!(
        expand_command(&commands, "/review a.rs\nand b.rs").as_deref(),
        Some("Review a.rs\nand b.rs.")
    );
    assert_eq!(
        expand_command(&commands, "/review").as_deref(),
        Some("Review .")
    );
    assert_eq!(expand_command(&commands, "/reviews a.rs"), None);
    assert_eq!(expand_command(&commands, "review a.rs"), None);
}
