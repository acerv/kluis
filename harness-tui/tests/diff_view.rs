// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use harness_core::tools::{diff_lines, Diff};
use harness_tui::{Action, App, ChangeKind, ChangesUpdate, FileChange, Status, Theme};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn app() -> App {
    let status = Status {
        sandbox: "sandboxed".into(),
        folder: "~/project".into(),
    };
    App::new("model", status, Theme::new(false))
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn type_line(app: &mut App, text: &str) -> Action {
    for ch in text.chars() {
        app.handle_event(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
}

/// The panel drawn on a `width` x `height` screen, row by row.
fn panel(app: &App, width: u16, height: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| app.render_diff_panel(f, f.area())).unwrap();
    let buf = term.backend().buffer();
    (0..height)
        .map(|y| {
            let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
            row.trim_end().to_owned()
        })
        .collect()
}

fn files() -> ChangesUpdate {
    let file = |path: &str, kind, lines| FileChange {
        path: PathBuf::from(path),
        kind,
        lines,
    };
    ChangesUpdate::Files {
        files: vec![
            file("img.png", ChangeKind::Added, None),
            file("src/main.rs", ChangeKind::Modified, Some((2, 1))),
            file("old.txt", ChangeKind::Deleted, Some((0, 3))),
        ],
        truncated: false,
    }
}

fn diff(path: &str, diff: Result<Diff, &'static str>) -> ChangesUpdate {
    ChangesUpdate::Diff {
        path: PathBuf::from(path),
        diff,
    }
}

#[test]
fn diff_command_toggles_the_panel_even_while_busy() {
    let mut app = app();
    assert!(!app.is_diff_open());
    assert_eq!(type_line(&mut app, "/diff"), Action::Redraw);
    assert!(app.is_diff_open());

    app.start_prompt("work");
    assert_eq!(type_line(&mut app, "/diff"), Action::Redraw);
    assert!(!app.is_diff_open());
    // Other commands still wait for the prompt to end.
    assert_eq!(type_line(&mut app, "/status"), Action::None);
}

#[test]
fn panel_lists_the_files_and_shows_the_selected_diff() {
    let mut app = app();
    type_line(&mut app, "/diff");
    assert!(panel(&app, 50, 12)[0].contains("Looking at the files…"));

    app.apply_changes(files());
    app.apply_changes(diff("img.png", Err("binary file")));
    let rows = panel(&app, 50, 12);
    assert_eq!(rows[0], "│ Changes 3 files +2 -4");
    assert_eq!(rows[1], "│ A img.png                                     ~");
    assert_eq!(rows[2], "│ M src/main.rs                             +2 -1");
    assert_eq!(rows[3], "│ D old.txt                                 +0 -3");
    assert_eq!(rows[4], "│ img.png");
    assert_eq!(rows[5], "│ binary file");
    assert_eq!(rows[11], "│ Alt+↑↓ file · PgUp/PgDn scroll");

    // Selecting another file waits for its diff, and only takes that one.
    let down = key(KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.handle_event(down), Action::Redraw);
    assert_eq!(panel(&app, 50, 12)[5], "│ …");
    app.apply_changes(diff("img.png", Err("binary file")));
    assert_eq!(panel(&app, 50, 12)[5], "│ …");
    app.apply_changes(diff("src/main.rs", Ok(diff_lines("a\nb\n", "a\nc\nd\n"))));
    let rows = panel(&app, 50, 12);
    assert_eq!(rows[4], "│ src/main.rs");
    assert_eq!(&rows[5..9], ["│  1  a", "│  2 -b", "│  2 +c", "│  3 +d"]);

    // The selection follows the file when the list changes.
    app.apply_changes(ChangesUpdate::Files {
        files: vec![FileChange {
            path: PathBuf::from("src/main.rs"),
            kind: ChangeKind::Modified,
            lines: Some((2, 1)),
        }],
        truncated: true,
    });
    let rows = panel(&app, 50, 12);
    assert_eq!(rows[0], "│ Changes 1 file +2 -1 (first 50000 files)");
    assert_eq!(rows[1], "│ M src/main.rs                             +2 -1");
    assert_eq!(rows[2], "│ src/main.rs");
    assert_eq!(rows[3], "│  1  a");
}

#[test]
fn keys_move_the_selection_and_scroll_only_while_open() {
    let mut app = app();
    let down = key(KeyCode::Down, KeyModifiers::CONTROL);
    let page_down = key(KeyCode::PageDown, KeyModifiers::NONE);
    let page_up = key(KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(app.handle_event(page_down.clone()), Action::None);

    type_line(&mut app, "/diff");
    app.apply_changes(files());
    let new: String = (1..=30).map(|n| format!("{n}\n")).collect();
    app.apply_changes(diff("img.png", Ok(diff_lines("", &new))));
    // 6 rows of diff on a 12 rows panel.
    assert!(panel(&app, 50, 12)[5].ends_with(" 1 +1"));
    assert_eq!(app.handle_event(page_up.clone()), Action::None);
    assert_eq!(app.handle_event(page_down.clone()), Action::Redraw);
    assert!(panel(&app, 50, 12)[5].ends_with(" 7 +7"));
    for _ in 0..10 {
        app.handle_event(page_down.clone());
    }
    // The last page stays full.
    let rows = panel(&app, 50, 12);
    assert!(rows[5].ends_with("25 +25"), "{rows:?}");
    assert!(rows[10].ends_with("30 +30"), "{rows:?}");
    assert_eq!(app.handle_event(page_up), Action::Redraw);

    assert_eq!(app.handle_event(down.clone()), Action::Redraw);
    assert_eq!(app.handle_event(down.clone()), Action::Redraw);
    assert_eq!(app.handle_event(down), Action::None);
}
