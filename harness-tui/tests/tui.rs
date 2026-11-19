// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use harness_core::tools::{Diff, DiffHunk, DiffLine};
use harness_core::ToolDetails;
use harness_tui::{
    flush_scrollback, Action, AgentUpdate, App, SlashCommand, Status, Theme, VIEWPORT_HEIGHT,
};
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::{Terminal, TerminalOptions, Viewport};

fn status() -> Status {
    Status {
        sandbox: "sandboxed".into(),
        folder: "~/project".into(),
        git_branch: Some("main".into()),
    }
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn shift(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT))
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
fn footer_shows_folder_branch_sandbox_tokens_and_model() {
    let mut term = inline_terminal(80, 14);
    let mut app = App::new("deepseek-chat", status(), Theme::new(true));

    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("~/project (main)"), "{screen}");
    assert!(screen.contains("sandboxed"), "{screen}");
    assert!(screen.contains("↑0 ↓0"), "{screen}");
    assert!(screen.contains("deepseek-chat"), "{screen}");
    // Editor is framed by horizontal rules.
    assert!(screen.contains("────────"), "{screen}");

    app.apply_update(AgentUpdate::TurnEnd {
        input_tokens: 120,
        output_tokens: 45,
    });
    app.set_model("gpt-4o");
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    let model_pos = screen.find("gpt-4o").expect("model in footer");
    let tokens_pos = screen.find("↑120 ↓45").expect("tokens in footer");
    assert!(
        model_pos < tokens_pos,
        "model should precede tokens: {screen}"
    );
}

#[test]
fn viewport_height_grows_with_newlines_and_wrapping() {
    let mut app = App::new("model", status(), Theme::new(true));
    assert_eq!(app.viewport_height(20), VIEWPORT_HEIGHT);

    // Explicit newline adds a row.
    app.handle_event(Event::Paste("line 1\nline 2".into()));
    assert_eq!(app.viewport_height(20), VIEWPORT_HEIGHT + 1);

    // Soft-wrapping past width - 2 (18 cols) adds rows too.
    app.handle_event(Event::Paste("\n".into()));
    app.handle_event(Event::Paste("a".repeat(40)));
    assert_eq!(app.viewport_height(20), VIEWPORT_HEIGHT + 4);

    // Submitting shrinks back to 1 editor row.
    app.handle_event(key(KeyCode::Enter));
    assert_eq!(app.viewport_height(20), VIEWPORT_HEIGHT);
}

#[test]
fn multiline_input_history_and_bracketed_paste() {
    let mut app = App::new("model", status(), Theme::new(true));

    // Type line 1, Shift+Enter for newline, type line 2, submit.
    for c in "first".chars() {
        app.handle_event(key(KeyCode::Char(c)));
    }
    assert_eq!(app.handle_event(shift(KeyCode::Enter)), Action::Redraw);
    for c in "second".chars() {
        app.handle_event(key(KeyCode::Char(c)));
    }
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Submit("first\nsecond".into())
    );

    // Bracketed paste normalizes CRLF and inserts at cursor.
    assert_eq!(
        app.handle_event(Event::Paste("a\r\nb".into())),
        Action::Redraw
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Submit("a\nb".into())
    );

    // Draft is preserved while navigating history with Up/Down.
    for c in "draft".chars() {
        app.handle_event(key(KeyCode::Char(c)));
    }
    assert_eq!(app.handle_event(key(KeyCode::Up)), Action::Redraw);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Submit("a\nb".into())
    );
    // Walk back through history, then forward to empty draft.
    assert_eq!(app.handle_event(key(KeyCode::Up)), Action::Redraw);
    assert_eq!(app.handle_event(key(KeyCode::Up)), Action::Redraw);
    assert_eq!(app.handle_event(key(KeyCode::Down)), Action::Redraw);
    assert_eq!(app.handle_event(key(KeyCode::Down)), Action::Redraw);
    assert_eq!(app.handle_event(key(KeyCode::Enter)), Action::Redraw);
}

#[test]
fn slash_commands() {
    let mut app = App::new("model", status(), Theme::new(true));

    for (input, expected) in [
        ("/model", Action::Slash(SlashCommand::Model(None))),
        (
            "/model gpt-4o",
            Action::Slash(SlashCommand::Model(Some("gpt-4o".into()))),
        ),
        ("/clear", Action::Slash(SlashCommand::Clear)),
        ("/quit", Action::Slash(SlashCommand::Quit)),
        ("/help", Action::Slash(SlashCommand::Help)),
    ] {
        app.handle_event(Event::Paste(input.into()));
        assert_eq!(app.handle_event(key(KeyCode::Enter)), expected, "{input}");
    }

    // Unknown slash command prints a local hint instead of submitting.
    app.handle_event(Event::Paste("/nope".into()));
    assert_eq!(app.handle_event(key(KeyCode::Enter)), Action::Redraw);
    let lines = app.take_scrollback();
    assert!(lines
        .iter()
        .any(|l| l.to_string().contains("unknown command")));
}

#[test]
fn ctrl_c_esc_ctrl_t_and_ctrl_d_behavior() {
    let mut app = App::new("model", status(), Theme::new(true));

    // Ctrl-T toggles thinking visibility.
    assert!(app.show_thinking());
    assert_eq!(app.handle_event(ctrl('t')), Action::Redraw);
    assert!(!app.show_thinking());
    assert_eq!(app.handle_event(ctrl('t')), Action::Redraw);
    assert!(app.show_thinking());

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
fn streaming_lines_thinking_and_tool_diff() {
    let mut term = inline_terminal(70, 20);
    let mut app = App::new("model", status(), Theme::new(true));

    app.start_prompt("fix it");
    flush_scrollback(&mut term, app.take_scrollback()).unwrap();

    // Thinking streams line-by-line into scrollback.
    app.apply_update(AgentUpdate::TurnStart);
    app.apply_update(AgentUpdate::ReasoningDelta("checking file\n".into()));
    let thought_lines = app.take_scrollback();
    assert!(thought_lines
        .iter()
        .any(|l| l.to_string().contains("checking file")));
    flush_scrollback(&mut term, thought_lines).unwrap();

    // Tool call shows clean pi-style header + colored edit diff.
    app.apply_update(AgentUpdate::ToolCall {
        name: "edit".into(),
        args: r#"{"path":"src/main.rs","edits":[]}"#.into(),
    });
    let diff = Diff {
        hunks: vec![DiffHunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            lines: vec![
                DiffLine::Removed("old".into()),
                DiffLine::Added("new".into()),
            ],
        }],
        first_changed_line: Some(1),
        truncated: false,
    };
    app.apply_update(AgentUpdate::ToolResult {
        content: r#"{"replacements":1}"#.into(),
        details: Some(ToolDetails::Edit(diff)),
    });
    let tool_lines = app.take_scrollback();
    let joined: String = tool_lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("edit src/main.rs"), "{joined}");
    assert!(joined.contains("-old"), "{joined}");
    assert!(joined.contains("+new"), "{joined}");
    flush_scrollback(&mut term, tool_lines).unwrap();

    // Text streams line-by-line above the editor, with partial line live.
    app.apply_update(AgentUpdate::TextDelta("Line one\nLine tw".into()));
    flush_scrollback(&mut term, app.take_scrollback()).unwrap();
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("Line one"), "{screen}");
    assert!(screen.contains("Line tw"), "{screen}");

    app.apply_update(AgentUpdate::TextDelta("o".into()));
    app.apply_update(AgentUpdate::Text("Line one\nLine two".into()));
    app.finish_prompt(Ok(()));
    flush_scrollback(&mut term, app.take_scrollback()).unwrap();
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("Line two"), "{screen}");
    assert!(!screen.contains("Working"), "{screen}");
}

#[test]
fn resize_and_no_color() {
    let mut term = inline_terminal(45, 14);
    let mut app = App::new("model", status(), Theme::new(false));
    assert_eq!(app.handle_event(Event::Resize(45, 14)), Action::Redraw);

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

#[test]
fn markdown_rendering_and_code_highlighting() {
    let mut app = App::new("model", status(), Theme::new(true));
    app.start_prompt("show rust");
    let _ = app.take_scrollback();

    // Code lines inside an unclosed fence commit to scrollback as `\n` arrives,
    // without committing a premature closing fence.
    app.apply_update(AgentUpdate::TextDelta(
        "# Heading\n\n- item 1\n- item 2\n\n```rust\nfn main() {\n".into(),
    ));
    let mid_lines = app.take_scrollback();
    let mid_plain: String = mid_lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(mid_plain.contains("# Heading"), "{mid_plain}");
    assert!(mid_plain.contains("item 1"), "{mid_plain}");
    assert!(mid_plain.contains("fn main() {"), "{mid_plain}");
    assert!(!mid_plain.ends_with("```"), "{mid_plain}");

    // Finish the code block (including an empty line and 4-space indented fence inside).
    let rest = "\n    let x = 42;\n    ```\n}\n```\n";
    let full = format!("# Heading\n\n- item 1\n- item 2\n\n```rust\nfn main() {{\n{rest}");
    app.apply_update(AgentUpdate::TextDelta(rest.into()));
    app.apply_update(AgentUpdate::Text(full.clone()));
    app.finish_prompt(Ok(()));

    let mut all_lines = mid_lines;
    all_lines.extend(app.take_scrollback());
    let plain: String = all_lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plain.contains("let x = 42;"), "{plain}");

    // Syntax highlighting must apply non-default styles to code spans when color is enabled.
    let has_colored_span = all_lines
        .iter()
        .flat_map(|l| &l.spans)
        .any(|s| s.style.fg.is_some_and(|c| c != Color::Reset));
    assert!(
        has_colored_span,
        "expected syntax-highlighted spans: {all_lines:?}"
    );

    // Under NO_COLOR, markdown rendering strips all styles and modifiers.
    let mut plain_app = App::new("model", status(), Theme::new(false));
    plain_app.start_prompt("show rust");
    let _ = plain_app.take_scrollback();
    plain_app.apply_update(AgentUpdate::Text(full));
    plain_app.finish_prompt(Ok(()));
    for line in plain_app.take_scrollback() {
        assert_eq!(line.style.fg, None);
        assert!(line.style.add_modifier.is_empty());
        for span in line.spans {
            assert_eq!(span.style.fg, None);
            assert!(span.style.add_modifier.is_empty());
        }
    }
}
