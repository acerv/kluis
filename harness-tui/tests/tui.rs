// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use harness_tui::{flush_scrollback, Action, AgentUpdate, App, Status, Theme, VIEWPORT_HEIGHT};
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::{Terminal, TerminalOptions, Viewport};

fn status() -> Status {
    Status {
        sandbox: "sandboxed".into(),
        folder: "~/project".into(),
    }
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn inline_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )
    .unwrap()
}

fn render_text(terminal: &Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn status_bar_shows_model_sandbox_tokens_and_folder() {
    let mut term = inline_terminal(80, 10);
    let mut app = App::new("deepseek-chat", status(), Theme::new(true));

    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("deepseek-chat"), "{screen}");
    assert!(screen.contains("sandboxed"), "{screen}");
    assert!(screen.contains("↑0 ↓0"), "{screen}");
    assert!(screen.contains("~/project"), "{screen}");

    app.apply_update(AgentUpdate::TurnEnd {
        input_tokens: 120,
        output_tokens: 45,
    });
    app.set_model("gpt-4o");
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("gpt-4o"), "{screen}");
    assert!(screen.contains("↑120 ↓45"), "{screen}");
}

#[test]
fn input_editing_and_submit() {
    let mut app = App::new("model", status(), Theme::new(true));
    for c in "helo".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(c))), Action::Redraw);
    }
    // Move left once and insert 'l' -> "hello".
    assert_eq!(app.handle_event(key(KeyCode::Left)), Action::Redraw);
    assert_eq!(app.handle_event(key(KeyCode::Char('l'))), Action::Redraw);
    // Enter submits trimmed text and clears input.
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Submit("hello".into())
    );
    // Submitting empty input does not emit Submit.
    assert_eq!(app.handle_event(key(KeyCode::Enter)), Action::Redraw);
}

#[test]
fn ctrl_c_esc_and_ctrl_d_behavior() {
    let mut app = App::new("model", status(), Theme::new(true));

    // Idle with non-empty input: Ctrl-C clears it; Ctrl-D does not quit.
    app.handle_event(key(KeyCode::Char('a')));
    assert_eq!(app.handle_event(ctrl('d')), Action::None);
    assert_eq!(app.handle_event(ctrl('c')), Action::Redraw);

    // Idle with empty input: Ctrl-C shows hint, Ctrl-D quits.
    assert_eq!(app.handle_event(ctrl('c')), Action::Redraw);
    assert_eq!(app.handle_event(ctrl('d')), Action::Quit);

    // Busy: both Ctrl-C and Esc cancel; Ctrl-D quits.
    app.start_prompt("work");
    assert!(app.is_busy());
    assert_eq!(app.handle_event(ctrl('c')), Action::Cancel);
    assert_eq!(app.handle_event(key(KeyCode::Esc)), Action::Cancel);
    assert_eq!(app.handle_event(ctrl('d')), Action::Quit);
}

#[test]
fn streaming_updates_and_scrollback_commit() {
    let mut term = inline_terminal(60, 12);
    let mut app = App::new("model", status(), Theme::new(true));

    app.start_prompt("check status");
    flush_scrollback(&mut term, app.take_scrollback()).unwrap();

    app.apply_update(AgentUpdate::TurnStart);
    app.apply_update(AgentUpdate::ToolCall {
        name: "bash".into(),
        args: r#"{"command":"ls"}"#.into(),
    });
    flush_scrollback(&mut term, app.take_scrollback()).unwrap();
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("Running bash"), "{screen}");
    assert!(screen.contains("> check status"), "{screen}");
    assert!(screen.contains("→ bash"), "{screen}");

    app.apply_update(AgentUpdate::ToolResult {
        content: r#"{"exit_code":0,"output":"ok"}"#.into(),
        details: None,
    });
    app.apply_update(AgentUpdate::TextDelta("All good".into()));
    flush_scrollback(&mut term, app.take_scrollback()).unwrap();
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("All good"), "{screen}");

    app.apply_update(AgentUpdate::Text("All good".into()));
    app.finish_prompt(Ok(()));
    assert!(!app.is_busy());
    flush_scrollback(&mut term, app.take_scrollback()).unwrap();
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("All good"), "{screen}");
    assert!(!screen.contains("Thinking"), "{screen}");
}

#[test]
fn resize_and_no_color() {
    let mut term = inline_terminal(40, 8);
    let mut app = App::new("model", status(), Theme::new(false));
    assert_eq!(app.handle_event(Event::Resize(40, 8)), Action::Redraw);

    app.start_prompt("hello");
    app.finish_prompt(Err("boom".into()));
    flush_scrollback(&mut term, app.take_scrollback()).unwrap();
    term.draw(|f| app.render(f)).unwrap();

    // Every cell must have default colors and no modifiers when NO_COLOR is active.
    let buf = term.backend().buffer();
    for cell in buf.content() {
        assert_eq!(cell.fg, Color::Reset, "fg color set: {cell:?}");
        assert_eq!(cell.bg, Color::Reset, "bg color set: {cell:?}");
        assert!(cell.modifier.is_empty(), "modifier set: {cell:?}");
    }
    let screen = render_text(&term);
    assert!(screen.contains("error: boom"), "{screen}");
}
