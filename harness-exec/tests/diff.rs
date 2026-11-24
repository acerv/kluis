// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use harness_exec::{count_lines, diff_lines, DiffHunk, DiffLine};

#[test]
fn diff_lines_keeps_context_around_changes() {
    let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
    let new = "a\nb\nc\nd\nE\nf\ng\nh\ni\nj\nk\n";
    let diff = diff_lines(old, new);
    assert_eq!(diff.first_changed_line, Some(5));
    assert!(!diff.truncated);
    assert_eq!(
        diff.hunks,
        vec![DiffHunk {
            old_start: 2,
            old_lines: 9,
            new_start: 2,
            new_lines: 10,
            lines: vec![
                DiffLine::Context("b".into()),
                DiffLine::Context("c".into()),
                DiffLine::Context("d".into()),
                DiffLine::Removed("e".into()),
                DiffLine::Added("E".into()),
                DiffLine::Context("f".into()),
                DiffLine::Context("g".into()),
                DiffLine::Context("h".into()),
                DiffLine::Context("i".into()),
                DiffLine::Context("j".into()),
                DiffLine::Added("k".into()),
            ],
        }]
    );
}

#[test]
fn diff_lines_of_a_new_and_a_removed_file() {
    let added = diff_lines("", "x\ny\n");
    assert_eq!(added.hunks.len(), 1);
    assert_eq!(added.hunks[0].old_start, 0);
    assert_eq!(added.hunks[0].new_start, 1);
    assert_eq!(added.hunks[0].new_lines, 2);

    let removed = diff_lines("x\ny\n", "");
    assert_eq!(removed.hunks[0].new_start, 0);
    assert_eq!(removed.hunks[0].old_lines, 2);
    assert!(diff_lines("same\n", "same\n").hunks.is_empty());
}

#[test]
fn count_lines_counts_added_and_removed_lines() {
    assert_eq!(count_lines("a\nb\nc\n", "a\nB\nc\nd\n"), (2, 1));
    assert_eq!(count_lines("", "x\ny\n"), (2, 0));
    assert_eq!(count_lines("x\ny\n", ""), (0, 2));
    assert_eq!(count_lines("same\n", "same\n"), (0, 0));
}
