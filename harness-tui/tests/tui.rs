// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use genai::chat::{ChatMessage, ChatRole, ContentPart, MessageContent, ToolCall, ToolResponse};
use harness_core::tools::{Diff, DiffHunk, DiffLine};
use harness_core::{Event as AgentEvent, SessionStore, TokenUsage, ToolDetails};
use harness_tui::{
    code_theme, flush_scrollback, last_assistant_text, osc52_sequence, Action, AgentUpdate, App,
    BannerLine, PromptSaver, Session, SlashCommand, Status, Theme, TRUNCATED_NOTICE,
    VIEWPORT_HEIGHT,
};
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier, Style};
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

fn shift(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT))
}

fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn type_str(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
}

fn inline_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    inline_terminal_with(width, height, VIEWPORT_HEIGHT)
}

fn inline_terminal_with(width: u16, height: u16, viewport: u16) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport),
        },
    )
    .unwrap()
}

/// Row index of the activity spinner, i.e. the line just above the editor.
fn spinner_row(terminal: &Terminal<TestBackend>) -> usize {
    render_text(terminal)
        .lines()
        .position(|line| line.contains("esc to cancel"))
        .expect("spinner line")
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

/// The footer line of a rendered `screen`.
fn footer_line(screen: &str) -> &str {
    screen
        .lines()
        .find(|line| line.ends_with("sandboxed · ~/project"))
        .expect(screen)
}

#[test]
fn footer_shows_model_context_sandbox_and_folder() {
    let mut term = inline_terminal(80, 14);
    let mut app = App::new("deepseek-chat", status(), Theme::new(true));

    // No size before the first request of the conversation.
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    let footer = footer_line(&screen);
    assert!(footer.starts_with("deepseek-chat (default)  "), "{screen}");
    assert!(footer.ends_with("sandboxed · ~/project"), "{screen}");
    assert!(screen.contains("────────"), "{screen}");

    // What the last request sent, cached or not.
    app.apply_update(AgentUpdate::TurnEnd(TokenUsage {
        input: 1_200,
        output: 300,
        cached: 800,
    }));
    app.set_model("gpt-4o");
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    let footer = footer_line(&screen);
    assert!(
        footer.starts_with("gpt-4o (default) · 2K context  "),
        "{screen}"
    );
    assert!(footer.ends_with("sandboxed · ~/project"), "{screen}");

    // Only the last request counts, not all of them.
    app.apply_update(AgentUpdate::TurnEnd(TokenUsage {
        input: 2_000_000,
        output: 350_000,
        cached: 1_500_000,
    }));
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(
        screen.contains("gpt-4o (default) · 3.5M context  "),
        "{screen}"
    );
}

#[test]
fn context_follows_the_conversation() {
    let mut app = App::new("model", status(), Theme::new(false));
    let mut term = inline_terminal(80, 14);
    let turn = TokenUsage {
        input: 1_000,
        output: 0,
        cached: 0,
    };

    // Another conversation: unknown, until its saved size comes along.
    app.apply_update(AgentUpdate::TurnEnd(turn));
    app.replay_history(&[ChatMessage::user("an older session")]);
    term.draw(|f| app.render(f)).unwrap();
    assert!(!render_text(&term).contains("context"));
    app.set_context(42_000);
    term.draw(|f| app.render(f)).unwrap();
    assert!(render_text(&term).contains("model (default) · 42K context"));

    app.apply_update(AgentUpdate::TurnEnd(turn));
    app.reset_session();
    term.draw(|f| app.render(f)).unwrap();
    assert!(!render_text(&term).contains("context"));
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

    // Submitting shrinks the editor back to one row, and starting a prompt
    // keeps the compact viewport height until live text actually wraps.
    app.handle_event(key(KeyCode::Enter));
    assert_eq!(app.viewport_height(20), VIEWPORT_HEIGHT);
    app.start_prompt("line 1\nline 2");
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
        ("/new", Action::Slash(SlashCommand::New)),
        ("/compact", Action::Slash(SlashCommand::Compact(None))),
        (
            "/compact keep api",
            Action::Slash(SlashCommand::Compact(Some("keep api".into()))),
        ),
        ("/retry", Action::Slash(SlashCommand::Retry)),
        ("/status", Action::Slash(SlashCommand::Status)),
        ("/output", Action::Slash(SlashCommand::Output)),
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
fn ctrl_c_esc_ctrl_o_and_ctrl_d_behavior() {
    let mut app = App::new("model", status(), Theme::new(true));

    // Ctrl-O toggles thinking visibility (idle and mid-stream).
    assert!(app.show_thinking());
    assert_eq!(app.handle_event(ctrl('o')), Action::Redraw);
    assert!(!app.show_thinking());
    assert_eq!(app.handle_event(ctrl('o')), Action::Redraw);
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
fn realtime_multiline_thinking_and_ctrl_o_toggle() {
    let mut app = App::new("model", status(), Theme::new(true));
    assert_eq!(app.handle_event(Event::Resize(20, 14)), Action::Redraw);

    app.start_prompt("think");
    let _ = app.take_scrollback();
    app.apply_update(AgentUpdate::TurnStart);

    // Long thinking paragraph without '\n' soft-wraps in real time at word
    // boundaries into scrollback as soon as it exceeds terminal width (20 cols).
    app.apply_update(AgentUpdate::ReasoningDelta(
        "Thinking about how to solve this cleanly".into(),
    ));
    let wrapped = app.take_scrollback();
    let rows: Vec<String> = wrapped.iter().map(ToString::to_string).collect();
    assert_eq!(rows, vec!["Thinking about how", "to solve this"]);

    // Hide thinking mid-turn with Ctrl-O: the live tail ("cleanly") is hidden
    // and new reasoning chunks buffer silently without emitting to scrollback.
    assert_eq!(app.handle_event(ctrl('o')), Action::Redraw);
    assert!(!app.show_thinking());
    app.apply_update(AgentUpdate::ReasoningDelta(" right now\n".into()));
    assert!(app.take_scrollback().is_empty());

    // Reveal thinking mid-turn with Ctrl-O: buffered reasoning flushes immediately.
    assert_eq!(app.handle_event(ctrl('o')), Action::Redraw);
    assert!(app.show_thinking());
    let revealed: Vec<String> = app
        .take_scrollback()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(revealed, vec!["cleanly right now"]);

    // Multi-line live Markdown preview grows viewport_height while streaming a
    // long paragraph before '\n' arrives, then shrinks back when finished.
    app.apply_update(AgentUpdate::TextDelta(
        "A long streaming markdown paragraph that wraps across multiple rows".into(),
    ));
    assert!(app.viewport_height(20) > VIEWPORT_HEIGHT);
    app.finish_prompt(Ok(()));
    assert_eq!(app.viewport_height(20), VIEWPORT_HEIGHT);
}

#[test]
fn streaming_preview_aligns_directly_below_scrollback() {
    let width = 50;
    let mut app = App::new("model", status(), Theme::new(true));
    app.start_prompt("go");
    let _ = app.take_scrollback();

    app.apply_update(AgentUpdate::TurnStart);
    app.apply_update(AgentUpdate::TextDelta(
        "streaming text that wraps cleanly across multiple preview rows in the viewport".into(),
    ));
    let height = app.viewport_height(width);
    assert!(height > VIEWPORT_HEIGHT);

    let mut term = inline_terminal_with(width, 24, height);
    term.draw(|f| app.render(f)).unwrap();
    let spinner = spinner_row(&term);

    // Live preview rows start at row 0 of the viewport (directly below
    // scrollback) and fill every row down to the spinner without blank gaps.
    let rows: Vec<String> = render_text(&term).lines().map(str::to_owned).collect();
    let preview: Vec<usize> = rows[..spinner]
        .iter()
        .enumerate()
        .filter(|(_, row)| !row.trim().is_empty())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(preview.first().copied(), Some(0), "{rows:#?}");
    assert_eq!(preview.last().copied(), Some(spinner - 1), "{rows:#?}");
    assert_eq!(preview.len(), spinner, "{rows:#?}");
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
    assert!(joined.contains("1 -old"), "{joined}");
    assert!(joined.contains("1 +new"), "{joined}");
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

#[test]
fn replay_history() {
    let mut app = App::new("deepseek-chat", status(), Theme::new(true));

    let history = vec![
        ChatMessage::user("check main"),
        ChatMessage::assistant(MessageContent::from_parts(vec![
            ContentPart::ReasoningContent("hidden thought".into()),
            ContentPart::ToolCall(ToolCall {
                call_id: "c1".into(),
                fn_name: "read".into(),
                fn_arguments: serde_json::json!({ "path": "src/main.rs" }),
                thought_signatures: None,
            }),
        ])),
        ChatMessage::from(ToolResponse::new("c1", r#"{"total_lines":42}"#)),
        ChatMessage::assistant("Looks good."),
    ];
    app.replay_history(&history);
    let lines = app.take_scrollback();
    let plain: String = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plain.contains("check main"), "{plain}");
    assert!(plain.contains("read src/main.rs"), "{plain}");
    assert!(plain.contains("42 lines"), "{plain}");
    assert!(plain.contains("Looks good."), "{plain}");
    assert!(!plain.contains("hidden thought"), "{plain}");
}

#[test]
fn session_picker_navigation_filtering_and_resume_slash() {
    use harness_core::SessionSummary;
    use std::path::PathBuf;

    let mut term = inline_terminal(90, 14);
    let mut app = App::new("deepseek-chat", status(), Theme::new(false));

    // `/resume` and `/resume <id>` parse into `SlashCommand::Resume`.
    type_str(&mut app, "/resume");
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Resume(None))
    );
    type_str(&mut app, "/resume 20261119");
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Resume(Some("20261119".into())))
    );

    let entries = vec![
        SessionSummary {
            id: "20261119-120000-aaaa".into(),
            path: PathBuf::from("/tmp/a.jsonl"),
            model: "deepseek-chat".into(),
            created_at: 100,
            updated_at: 100,
            message_count: 4,
            first_prompt: Some("fix build error".into()),
        },
        SessionSummary {
            id: "20261119-110000-bbbb".into(),
            path: PathBuf::from("/tmp/b.jsonl"),
            model: "claude-sonnet".into(),
            created_at: 50,
            updated_at: 50,
            message_count: 1,
            first_prompt: Some("refactor parser".into()),
        },
    ];
    app.open_picker(entries.clone(), Some("20261119-120000-aaaa".into()));
    assert!(app.is_picker_open());
    assert_eq!(app.viewport_height(90), 6);

    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("Resume session"), "{screen}");
    assert!(screen.contains("❯ fix build error"), "{screen}");
    assert!(screen.contains("(current)"), "{screen}");
    assert!(screen.contains("refactor parser"), "{screen}");
    // NO_COLOR theme keeps all picker cells unstyled.
    for cell in term.backend().buffer().content() {
        assert_eq!(cell.fg, Color::Reset, "fg set: {cell:?}");
        assert!(cell.modifier.is_empty(), "modifier set: {cell:?}");
    }

    // Move selection down and pick second session.
    assert_eq!(app.handle_event(key(KeyCode::Down)), Action::Redraw);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::ResumeSession("20261119-110000-bbbb".into())
    );
    assert!(!app.is_picker_open());

    // Live substring filtering + Esc cancel.
    app.open_picker(entries, None);
    type_str(&mut app, "claude");
    term.draw(|f| app.render(f)).unwrap();
    let filtered = render_text(&term);
    assert!(filtered.contains("filter: claude"), "{filtered}");
    assert!(filtered.contains("refactor parser"), "{filtered}");
    assert!(!filtered.contains("fix build error"), "{filtered}");

    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::ResumeSession("20261119-110000-bbbb".into())
    );

    app.open_picker(vec![], None);
    assert_eq!(app.handle_event(key(KeyCode::Esc)), Action::Redraw);
    assert!(!app.is_picker_open());
}

#[test]
fn persistent_history_round_trip_multiline_dedup_and_compaction() {
    use harness_tui::{History, MAX_HISTORY};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kluis").join("history");

    // Missing file loads empty without creating anything on disk.
    let mut history = History::load(&path);
    assert!(history.entries().is_empty());
    assert!(!path.exists());

    // Blank entries and consecutive duplicates are ignored; multiline prompts round-trip.
    history.record("   ").unwrap();
    assert!(!path.exists());
    history.record("first prompt").unwrap();
    history.record("first prompt").unwrap();
    history.record("line one\nline two").unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "history file must be 0600, got {mode:o}");

    // Simulate a crash leaving a partial line with no trailing newline.
    let mut raw = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    raw.write_all(b"\"truncated").unwrap();
    drop(raw);

    // Reloading ignores the corrupt tail and cleanly separates the next record.
    let mut reloaded = History::load(&path);
    assert_eq!(
        reloaded.entries(),
        vec!["first prompt", "line one\nline two"]
    );
    reloaded.record("after crash").unwrap();

    let after = History::load(&path);
    assert_eq!(
        after.entries(),
        vec!["first prompt", "line one\nline two", "after crash"]
    );

    // Seeding `App` recalls persisted multiline prompts via Up/Down.
    let mut app = App::new("model", status(), Theme::new(false));
    app.set_history(after.entries());
    assert_eq!(app.handle_event(key(KeyCode::Up)), Action::Redraw);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Submit("after crash".into())
    );
    assert_eq!(app.handle_event(key(KeyCode::Up)), Action::Redraw);
    assert_eq!(app.handle_event(key(KeyCode::Up)), Action::Redraw);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Submit("line one\nline two".into())
    );

    // Writing past compaction threshold trims the file back down to MAX_HISTORY lines.
    let compact_path = dir.path().join("compact");
    let mut h = History::load(&compact_path);
    for i in 0..(MAX_HISTORY * 2 + 5) {
        h.record(&format!("cmd-{i}")).unwrap();
    }
    let lines: Vec<String> = std::fs::read_to_string(&compact_path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(lines.len() <= MAX_HISTORY + 10, "got {} lines", lines.len());
    let final_entries = History::load(&compact_path).entries();
    assert_eq!(final_entries.len(), MAX_HISTORY);
    assert_eq!(
        final_entries.last().map(String::as_str),
        Some(format!("cmd-{}", MAX_HISTORY * 2 + 4).as_str())
    );

    // `ensure_file` rejects non-regular paths (such as a directory).
    assert!(History::ensure_file(dir.path()).is_err());

    // Oversized prompts are ignored rather than bloating or evicting history.
    h.record(&"x".repeat(32 * 1024)).unwrap();
    assert_eq!(History::load(&compact_path).entries(), final_entries);
}

#[test]
fn thinking_effort_slash_keybinding_and_footer() {
    use harness_core::Thinking;

    let mut term = inline_terminal(80, 14);
    let mut app = App::new("claude-sonnet-4-5", status(), Theme::new(false));

    // `/thinking` and `/thinking <level>` parse into `SlashCommand::Thinking`.
    for ch in "/thinking".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Thinking(None))
    );
    for ch in "/thinking high".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Thinking(Some("high".into())))
    );

    // Shift+Tab cycles default -> off -> low -> medium -> high -> default -> off and updates footer.
    for expected in [
        Thinking::Off,
        Thinking::Low,
        Thinking::Medium,
        Thinking::High,
        Thinking::Default,
        Thinking::Off,
    ] {
        assert_eq!(
            app.handle_event(shift(KeyCode::BackTab)),
            Action::SetThinking(expected)
        );
        assert_eq!(app.thinking(), expected);
    }

    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("claude-sonnet-4-5 (off)  "), "{screen}");
    assert!(screen.contains("Thinking: off"), "{screen}");

    // Footer model color changes by thinking effort when color is enabled.
    let mut colored = App::new("deepseek-flash", status(), Theme::new(true));
    for (level, expected_fg) in [
        (Thinking::Default, Color::Green),
        (Thinking::Off, Color::Gray),
        (Thinking::Low, Color::Cyan),
        (Thinking::Medium, Color::Yellow),
        (Thinking::High, Color::Indexed(208)),
    ] {
        colored.set_thinking(level);
        term.draw(|f| colored.render(f)).unwrap();
        let buf = term.backend().buffer();
        let footer_y = VIEWPORT_HEIGHT - 1;
        assert_eq!(buf[(0, footer_y)].symbol(), "d");
        assert_eq!(buf[(0, footer_y)].fg, expected_fg, "level: {level}");
    }
}

#[test]
fn slash_command_autocomplete_popup_and_tab_completion() {
    let mut app = App::new("deepseek-chat", status(), Theme::new(false));
    assert!(!app.is_completion_open());

    // Typing `/` opens the popup with all commands and expands the viewport.
    app.handle_event(key(KeyCode::Char('/')));
    assert!(app.is_completion_open());
    let h = app.viewport_height(80);
    assert!(h > VIEWPORT_HEIGHT, "viewport did not grow: {h}");

    let mut term = inline_terminal_with(80, 16, h);
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("❯ /model [name]"), "{screen}");
    assert!(screen.contains("/thinking [level]"), "{screen}");
    assert!(screen.contains("/resume [id]"), "{screen}");
    assert!(screen.contains("/copy"), "{screen}");
    assert!(screen.contains("/new"), "{screen}");
    assert!(screen.contains("/retry"), "{screen}");
    for cell in term.backend().buffer().content() {
        assert_eq!(cell.fg, Color::Reset, "fg set under NO_COLOR: {cell:?}");
        assert!(cell.modifier.is_empty(), "modifier set: {cell:?}");
    }

    // Typing `t` narrows to `/thinking`, Tab inserts `/thinking ` and offers effort levels.
    app.handle_event(key(KeyCode::Char('t')));
    assert_eq!(app.handle_event(key(KeyCode::Tab)), Action::Redraw);
    assert!(app.is_completion_open());
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("❯ /thinking default"), "{screen}");
    assert!(screen.contains("/thinking off"), "{screen}");
    assert!(screen.contains("/thinking high"), "{screen}");

    // Down selects `off`, Enter submits `/thinking off` and closes popup.
    assert_eq!(app.handle_event(key(KeyCode::Down)), Action::Redraw);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Thinking(Some("off".into())))
    );
    assert!(!app.is_completion_open());

    // Partial no-arg command `/n` and uppercase `/NEW` both resolve to `/new`.
    app.handle_event(key(KeyCode::Char('/')));
    app.handle_event(key(KeyCode::Char('n')));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::New)
    );
    assert!(!app.is_completion_open());

    // Partial `/s` resolves to `/status`.
    app.handle_event(key(KeyCode::Char('/')));
    app.handle_event(key(KeyCode::Char('s')));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Status)
    );

    app.handle_event(Event::Paste("/NEW".into()));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::New)
    );

    // `/clear` is no longer a valid command.
    app.handle_event(Event::Paste("/clear".into()));
    assert_eq!(app.handle_event(key(KeyCode::Enter)), Action::Redraw);
    assert!(app
        .take_scrollback()
        .iter()
        .any(|l| l.to_string().contains("unknown command: /clear")));

    // Esc dismisses popup; cursor movement keeps it dismissed until text changes.
    app.handle_event(key(KeyCode::Char('/')));
    assert!(app.is_completion_open());
    assert_eq!(app.handle_event(key(KeyCode::Esc)), Action::Redraw);
    assert!(!app.is_completion_open());
    app.handle_event(key(KeyCode::Left));
    assert!(!app.is_completion_open());
    app.handle_event(key(KeyCode::Right));
    app.handle_event(key(KeyCode::Char('r')));
    assert!(app.is_completion_open());
}

#[test]
fn live_bash_preview_and_elapsed_spinner_timer() {
    use std::time::Duration;

    let mut app = App::new("deepseek-chat", status(), Theme::new(false));
    app.start_prompt("run build");
    let _ = app.take_scrollback();

    app.apply_update(AgentUpdate::ToolCall {
        name: "bash".into(),
        args: r#"{"command":"cargo test"}"#.into(),
    });
    app.set_elapsed(Duration::from_secs(12));
    app.apply_update(AgentUpdate::ToolOutput(
        "compiling\rbuilding 50%\rbuilding 100%\nline 2\nline 3".into(),
    ));

    let h = app.viewport_height(80);
    assert!(
        h > VIEWPORT_HEIGHT,
        "live preview should grow viewport: {h}"
    );
    let mut term = inline_terminal_with(80, 14, h);
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("Running bash… 12s"), "{screen}");
    assert!(screen.contains("building 100%"), "{screen}");
    assert!(
        !screen.contains("compiling"),
        "CR overwrite failed: {screen}"
    );
    assert!(screen.contains("line 2"), "{screen}");
    assert!(screen.contains("line 3"), "{screen}");

    // >= 60s formats as minutes + zero-padded seconds.
    app.set_elapsed(Duration::from_secs(65));
    term.draw(|f| app.render(f)).unwrap();
    assert!(render_text(&term).contains("Running bash… 1m05s"));

    // Completing the tool clears the transient live preview.
    app.apply_update(AgentUpdate::ToolResult {
        content: r#"{"output":"done\n","exit_code":0}"#.into(),
        details: None,
    });
    assert_eq!(app.viewport_height(80), VIEWPORT_HEIGHT);
}

#[test]
fn copy_command_extracts_last_assistant_reply_and_formats_osc52() {
    let mut app = App::new("deepseek-chat", status(), Theme::new(false));
    app.handle_event(Event::Paste("/copy".into()));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Copy)
    );

    // Partial `/c` completes to `/copy` on Enter.
    app.handle_event(key(KeyCode::Char('/')));
    app.handle_event(key(KeyCode::Char('c')));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Copy)
    );

    // Empty or user-only history has nothing to copy.
    assert_eq!(last_assistant_text(&[]), None);
    assert_eq!(last_assistant_text(&[ChatMessage::user("hi")]), None);

    // Skips trailing tool-only and reasoning-only assistant turns.
    let history = vec![
        ChatMessage::user("first"),
        ChatMessage::assistant("earlier reply"),
        ChatMessage::user("second"),
        ChatMessage::assistant(MessageContent::from_parts(vec![
            ContentPart::ReasoningContent("thinking...".into()),
            ContentPart::Text("final answer".into()),
        ])),
        ChatMessage::assistant(MessageContent::from_parts(vec![ContentPart::ToolCall(
            ToolCall {
                call_id: "c1".into(),
                fn_name: "read".into(),
                fn_arguments: serde_json::json!({ "path": "src/main.rs" }),
                thought_signatures: None,
            },
        )])),
        ChatMessage::from(ToolResponse::new("c1", r#"{"content":"ok"}"#)),
    ];
    assert_eq!(
        last_assistant_text(&history).as_deref(),
        Some("final answer")
    );

    // OSC 52 sequence base64-encodes UTF-8 into the system clipboard (`c`).
    assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(osc52_sequence("ciao ✓"), "\x1b]52;c;Y2lhbyDinJM=\x07");
}

#[test]
fn model_picker_navigation_filtering_and_selection() {
    use harness_core::ModelInfo;

    let type_str = |app: &mut App, s: &str| {
        for ch in s.chars() {
            app.handle_event(key(KeyCode::Char(ch)));
        }
    };

    let mut term = inline_terminal(90, 14);
    let mut app = App::new("deepseek-chat", status(), Theme::new(false));

    // `/model` and `/model <name>` parse into `SlashCommand::Model`.
    type_str(&mut app, "/model");
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Model(None))
    );
    type_str(&mut app, "/model gpt-4o");
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Model(Some("gpt-4o".into())))
    );

    let models = vec![
        ModelInfo {
            id: "claude-sonnet-4-5".into(),
            provider: "anthropic".into(),
        },
        ModelInfo {
            id: "deepseek-chat".into(),
            provider: "deepseek".into(),
        },
        ModelInfo {
            id: "ollama::qwen2.5-coder".into(),
            provider: "ollama".into(),
        },
    ];

    // Opening the picker pre-selects the currently active model and marks it `(current)`.
    app.open_model_picker(&models);
    assert!(app.is_picker_open());
    assert_eq!(app.viewport_height(90), 7);

    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("Select model"), "{screen}");
    assert!(
        screen.contains("❯ deepseek-chat · deepseek (current)"),
        "{screen}"
    );
    assert!(screen.contains("claude-sonnet-4-5 · anthropic"), "{screen}");
    for cell in term.backend().buffer().content() {
        assert_eq!(cell.fg, Color::Reset, "fg set under NO_COLOR: {cell:?}");
        assert!(cell.modifier.is_empty(), "modifier set: {cell:?}");
    }

    // Pressing Down selects the next model (namespaced) and Enter emits `Action::SelectModel`.
    assert_eq!(app.handle_event(key(KeyCode::Down)), Action::Redraw);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::SelectModel("ollama::qwen2.5-coder".into())
    );
    assert!(!app.is_picker_open());

    // Filtering by provider name narrows results and Enter picks the filtered match.
    app.open_model_picker(&models);
    type_str(&mut app, "ANTHROPIC");
    term.draw(|f| app.render(f)).unwrap();
    let filtered = render_text(&term);
    assert!(filtered.contains("filter: ANTHROPIC  (1/3)"), "{filtered}");
    assert!(filtered.contains("❯ claude-sonnet-4-5"), "{filtered}");
    assert!(!filtered.contains("deepseek-chat · deepseek"), "{filtered}");
    assert!(!filtered.contains("ollama::qwen2.5-coder"), "{filtered}");

    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::SelectModel("claude-sonnet-4-5".into())
    );
    assert!(!app.is_picker_open());

    // Explicit `<provider>::` model name still highlights and badges the un-namespaced entry.
    let mut namespaced = App::new("deepseek::deepseek-chat", status(), Theme::new(false));
    namespaced.open_model_picker(&models);
    term.draw(|f| namespaced.render(f)).unwrap();
    assert!(
        render_text(&term).contains("❯ deepseek-chat · deepseek (current)"),
        "{}",
        render_text(&term)
    );

    // Esc cancels cleanly; Ctrl-D quits.
    app.open_model_picker(&models);
    assert_eq!(app.handle_event(key(KeyCode::Esc)), Action::Redraw);
    assert!(!app.is_picker_open());

    app.open_model_picker(&models);
    assert_eq!(app.handle_event(ctrl('d')), Action::Quit);
    assert!(!app.is_picker_open());
}

#[test]
fn banner_puts_info_next_to_logo() {
    let theme = Theme::new(true);
    let mut app = App::new("model", status(), theme);
    app.push_banner(&[
        BannerLine::new("kluis v1"),
        BannerLine::warning("openai · no credentials"),
        BannerLine::new("sandbox: no sandbox"),
        BannerLine::new("context: no AGENTS.md"),
    ]);
    let lines = app.take_scrollback();
    let text: Vec<_> = lines.iter().map(|l| l.to_string()).collect();
    assert_eq!(
        text,
        [
            "",
            "█ ▄▀ █     kluis v1",
            "█▀▄  █     openai · no credentials",
            "▀  ▀ ▀▀▀   sandbox: no sandbox",
            "           context: no AGENTS.md",
            "",
        ]
    );
    assert_eq!(lines[1].spans[1].style, theme.muted);
    assert_eq!(lines[2].spans[1].style, theme.error);
    // Each logo row has its own color, plain without colors.
    for (row, style) in theme.logo.iter().enumerate() {
        assert_eq!(lines[row + 1].spans[0].style, *style);
    }
    assert_ne!(theme.logo[0], theme.logo[2]);
    assert_eq!(Theme::new(false).logo, [Style::default(); 3]);

    // Fewer lines than the logo: the logo rows stay whole, without padding.
    app.push_banner(&[BannerLine::new("kluis v1")]);
    let text: Vec<_> = app
        .take_scrollback()
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(text[2..], ["█▀▄  █", "▀  ▀ ▀▀▀", ""]);
}

#[test]
fn ask_picks_an_option_or_takes_typed_text() {
    let mut app = App::new("model", status(), Theme::new(false));
    let mut term = inline_terminal(90, 20);
    let options = ["yes".to_owned(), "no".to_owned()];
    app.start_prompt("go");

    app.ask("Proceed?", &options);
    assert!(app.is_picker_open());
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("Answer:"), "{screen}");
    assert!(screen.contains("❯ yes"), "{screen}");
    assert!(screen.contains("no"), "{screen}");

    assert_eq!(app.handle_event(key(KeyCode::Down)), Action::Redraw);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Answer("no".into())
    );
    assert!(!app.is_picker_open());

    // Typing does not filter the options: it is the answer.
    app.ask("Proceed?", &options);
    type_str(&mut app, "ye");
    assert_eq!(
        app.handle_event(Event::Paste("s, but later".into())),
        Action::Redraw
    );
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("Answer: yes, but later"), "{screen}");
    assert!(screen.contains("no"), "{screen}");
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Answer("yes, but later".into())
    );
}

#[test]
fn ask_without_options_and_cancel() {
    let mut app = App::new("model", status(), Theme::new(false));
    let mut term = inline_terminal(30, 20);
    app.start_prompt("go");

    app.ask("Name?", &[]);
    // Nothing to pick: Enter waits for text.
    assert_eq!(app.handle_event(key(KeyCode::Enter)), Action::None);
    let long = "x".repeat(100) + "END";
    type_str(&mut app, &long);
    term.draw(|f| app.render(f)).unwrap();
    // The end of a long answer stays in view.
    let screen = render_text(&term);
    assert!(screen.contains("END"), "{screen}");

    // The model waits for the answer, so leaving cancels the prompt.
    assert_eq!(app.handle_event(key(KeyCode::Esc)), Action::Cancel);
    assert!(!app.is_picker_open());
}

#[test]
fn ask_shows_a_long_question_in_full() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("go");
    let question = "Which database should the service use?\nPostgres is already deployed.";

    app.ask(question, &[]);
    let lines: Vec<String> = app
        .take_scrollback()
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert!(
        lines.iter().any(|l| l == "  Postgres is already deployed."),
        "{lines:?}"
    );
}

/// Run a `todo` call with `todos` (a JSON list) through `app`.
fn apply_todo(app: &mut App, todos: &str) {
    let args = format!(r#"{{"todos":{todos}}}"#);
    let parsed: harness_core::tools::TodoTool = serde_json::from_str(&args).unwrap();
    let count = parsed.todos.len();
    app.apply_update(AgentUpdate::ToolCall {
        name: "todo".into(),
        args,
    });
    app.apply_update(AgentUpdate::ToolResult {
        content: format!(r#"{{"count":{count}}}"#),
        details: Some(ToolDetails::Todo(parsed.todos)),
    });
}

const PLAN: &str = r#"[
    {"content":"plan","status":"completed"},
    {"content":"build","status":"in_progress"},
    {"content":"test","status":"pending"}]"#;

const DONE: &str = r#"[{"content":"plan","status":"completed"}]"#;

/// Draw `app` on a fresh terminal tall enough for its viewport.
fn draw_app(app: &App) -> Terminal<TestBackend> {
    let height = app.viewport_height(40);
    let mut term = inline_terminal_with(40, height, height);
    term.draw(|f| app.render(f)).unwrap();
    term
}

#[test]
fn plan_shows_above_the_editor_instead_of_scrollback() {
    let mut app = App::new("model", status(), Theme::new(true));
    app.start_prompt("go");
    app.take_scrollback();
    apply_todo(&mut app, PLAN);
    assert!(app.take_scrollback().is_empty());

    let term = draw_app(&app);
    let screen = render_text(&term);
    let rows: Vec<&str> = screen.lines().collect();
    let plan = rows.iter().position(|r| *r == "Plan 1/3").expect(&screen);
    assert_eq!(
        rows[plan + 1..plan + 4],
        ["  ✓ plan", "  ▸ build", "  ○ test"]
    );
    // Right over the editor's top rule.
    assert!(rows[plan + 4].starts_with('─'), "{screen}");

    let buf = term.backend().buffer();
    let current = &buf[(4, (plan + 2) as u16)];
    assert_eq!(current.fg, Color::Yellow);
    assert!(current.modifier.contains(Modifier::BOLD));
    let done = &buf[(4, (plan + 1) as u16)];
    assert!(done.modifier.contains(Modifier::CROSSED_OUT));
}

#[test]
fn plan_stays_between_prompts_until_done() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("go");
    apply_todo(&mut app, PLAN);
    app.finish_prompt(Ok(()));
    assert!(render_text(&draw_app(&app)).contains("Plan 1/3"));

    app.start_prompt("go on");
    apply_todo(&mut app, DONE);
    // Done, but still in view until the prompt ends.
    assert!(render_text(&draw_app(&app)).contains("Plan 1/1"));
    app.finish_prompt(Ok(()));
    assert!(!render_text(&draw_app(&app)).contains("Plan"));
}

#[test]
fn cancelling_clears_an_unfinished_plan() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("go");
    apply_todo(&mut app, PLAN);
    app.cancel_prompt();
    assert!(!render_text(&draw_app(&app)).contains("Plan"));
}

#[test]
fn long_plan_shows_the_items_around_the_current_one() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("go");
    let items: Vec<String> = (1..=12)
        .map(|i| {
            let status = match i {
                ..5 => "completed",
                5 => "in_progress",
                _ => "pending",
            };
            format!(r#"{{"content":"step {i}","status":"{status}"}}"#)
        })
        .collect();
    apply_todo(&mut app, &format!("[{}]", items.join(",")));

    let screen = render_text(&draw_app(&app));
    assert!(screen.contains("Plan 4/12"), "{screen}");
    assert!(!screen.contains("step 3\n"), "{screen}");
    assert!(screen.contains("✓ step 4"), "{screen}");
    assert!(screen.contains("▸ step 5"), "{screen}");
    assert!(screen.contains("○ step 11"), "{screen}");
    assert!(!screen.contains("step 12"), "{screen}");
    assert!(screen.contains("… +4 more"), "{screen}");
}

#[test]
fn failed_todo_call_shows_its_error() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("go");
    app.take_scrollback();
    app.apply_update(AgentUpdate::ToolCall {
        name: "todo".into(),
        args: "{}".into(),
    });
    app.apply_update(AgentUpdate::ToolResult {
        content: r#"{"error":"missing field `todos`"}"#.into(),
        details: None,
    });
    let text: Vec<String> = app
        .take_scrollback()
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(text[..2], ["todo", "  error: missing field `todos`"]);
}

#[test]
fn replay_restores_an_unfinished_plan() {
    let call = |id: &str, todos: &str, response: &str| {
        let args: serde_json::Value =
            serde_json::from_str(&format!(r#"{{"todos":{todos}}}"#)).unwrap();
        vec![
            ChatMessage::assistant(MessageContent::from_parts(vec![ContentPart::ToolCall(
                ToolCall {
                    call_id: id.into(),
                    fn_name: "todo".into(),
                    fn_arguments: args,
                    thought_signatures: None,
                },
            )])),
            ChatMessage::from(ToolResponse::new(id, response)),
        ]
    };

    let mut app = App::new("model", status(), Theme::new(false));
    app.replay_history(&call("c1", PLAN, r#"{"count":3}"#));
    let scrollback: Vec<String> = app
        .take_scrollback()
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert!(
        !scrollback.iter().any(|l| l.contains("build")),
        "{scrollback:?}"
    );
    assert!(render_text(&draw_app(&app)).contains("▸ build"));

    // Another conversation takes its place, as with `/resume`: not its plan.
    app.replay_history(&[ChatMessage::user("something else")]);
    assert!(!render_text(&draw_app(&app)).contains("Plan"));

    let mut app = App::new("model", status(), Theme::new(false));
    app.replay_history(&call("c1", DONE, r#"{"count":1}"#));
    assert!(!render_text(&draw_app(&app)).contains("Plan"));

    // Failed todo call during replay shows its error instead of applying invalid args.
    let mut app = App::new("model", status(), Theme::new(false));
    app.replay_history(&call(
        "c1",
        PLAN,
        r#"{"error":"only one todo can be in_progress"}"#,
    ));
    assert!(!render_text(&draw_app(&app)).contains("Plan"));
    let scrollback: Vec<String> = app
        .take_scrollback()
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(
        scrollback[..2],
        ["todo", "  error: only one todo can be in_progress"]
    );
}

#[test]
fn reply_cut_at_the_token_limit_shows_notice() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("go");
    app.apply_update(AgentUpdate::Text("half a sen".into()));
    app.apply_update(AgentUpdate::Truncated);
    let lines = scrollback_lines(&mut app);
    assert!(lines.iter().any(|l| l == TRUNCATED_NOTICE), "{lines:?}");
}

#[test]
fn prompt_is_saved_while_it_runs() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path(), home.path());
    let roles = |store: &SessionStore| -> Vec<ChatRole> {
        let data = store.latest().unwrap().unwrap();
        data.messages.into_iter().map(|m| m.role).collect()
    };
    let writer = store.create("model");
    let mut session = Session::new(store.clone(), writer, TokenUsage::default(), 0);
    let turn = [ChatMessage::user("go"), ChatMessage::assistant("on it")];

    // A prompt stopped half way, as by a signal, keeps what it reported.
    let mut saver = PromptSaver::new(Some(&mut session), &[]);
    saver.on_event(&AgentEvent::Messages(&turn));
    drop(saver);
    assert_eq!(roles(&store), [ChatRole::User, ChatRole::Assistant]);

    // A compaction replaces what was saved, and the end adds the rest.
    let summary = [ChatMessage::user("summary")];
    let history = [summary[0].clone(), turn[1].clone()];
    let mut saver = PromptSaver::new(Some(&mut session), &turn);
    saver.on_event(&AgentEvent::Compacted {
        before: 2,
        messages: &summary,
    });
    let usage = TokenUsage {
        input: 7,
        output: 3,
        cached: 0,
    };
    saver.finish(&history, usage, 7).unwrap();
    assert_eq!(roles(&store), [ChatRole::User, ChatRole::Assistant]);
    let data = store.latest().unwrap().unwrap();
    assert_eq!(data.messages[0].content.first_text(), Some("summary"));
    assert_eq!((data.usage, data.context), (usage, 7));
    assert_eq!((session.usage(), session.context()), (usage, 7));
}

#[test]
fn retry_keeps_partial_text_and_shows_notice() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("go");
    app.take_scrollback();
    app.apply_update(AgentUpdate::TextDelta("half an ans".into()));
    app.apply_update(AgentUpdate::Retry {
        attempt: 1,
        delay: std::time::Duration::from_secs(2),
        reason: "HTTP 429 Too Many Requests".into(),
    });

    let text: Vec<String> = app
        .take_scrollback()
        .iter()
        .map(|l| l.to_string())
        .collect();
    let notice = text
        .iter()
        .position(|l| l == "[HTTP 429 Too Many Requests · retry 1 in 2s]");
    let partial = text.iter().position(|l| l.contains("half an ans"));
    assert!(partial < notice && notice.is_some(), "{text:?}");
}

#[test]
fn git_guard_runs_only_after_tools_that_change_files() {
    use harness_sandbox::{GitGuard, Sandbox};
    use harness_tui::git_notices;

    let dir = tempfile::tempdir().unwrap();
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(dir.path())).unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let prompt = |tool: &str| {
        let call = ToolCall {
            call_id: "c1".into(),
            fn_name: tool.into(),
            fn_arguments: serde_json::json!({}),
            thought_signatures: None,
        };
        vec![ChatMessage::assistant(MessageContent::from_tool_calls(
            vec![call],
        ))]
    };

    // Reading cannot create a repository: no scan.
    assert!(git_notices(Some(&mut guard), &prompt("read")).is_empty());
    assert!(dir.path().join(".git").exists());

    let notices = git_notices(Some(&mut guard), &prompt("bash"));
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains(".git.kl-disabled"), "{notices:?}");
    assert!(!dir.path().join(".git").exists());
}

#[test]
fn git_guard_warns_about_folders_it_cannot_read() {
    use std::os::unix::fs::PermissionsExt;

    use harness_sandbox::{GitGuard, Sandbox};
    use harness_tui::git_notices;

    let dir = tempfile::tempdir().unwrap();
    let mut guard = GitGuard::new(&Sandbox::new().allow_write(dir.path())).unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let mode =
        |mode| std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(mode)).unwrap();
    let call = ToolCall {
        call_id: "c1".into(),
        fn_name: "bash".into(),
        fn_arguments: serde_json::json!({}),
        thought_signatures: None,
    };
    let prompt = [ChatMessage::assistant(MessageContent::from_tool_calls(
        vec![call],
    ))];

    mode(0o300);
    let notices = git_notices(Some(&mut guard), &prompt);
    mode(0o700);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].starts_with("[warning: "), "{notices:?}");
    assert!(notices[0].contains(&*sub.to_string_lossy()), "{notices:?}");
}

#[test]
fn printable_text_sends_no_escape_sequences() {
    use std::borrow::Cow;

    use harness_tui::printable;

    // OSC 52 sets the clipboard; CSI in its one-byte form; a carriage
    // return could overwrite the line with other text.
    let text = "a\x1b]52;c;cHduZWQ=\x07b \u{9b}31mc\rd\n\te";
    assert_eq!(printable(text), "a]52;c;cHduZWQ=b 31mcd\n\te");
    assert!(matches!(printable("plain\ntext"), Cow::Borrowed(_)));
}

/// The colors of the code in a ```sh block, as `app` shows it.
fn code_colors(app: &mut App) -> Vec<Color> {
    app.replay_history(&[ChatMessage::assistant("```sh\nls -la /tmp\n```")]);
    let lines = app.take_scrollback();
    let code = lines
        .iter()
        .find(|line| line.to_string().contains("ls -la"))
        .expect("code line");
    code.spans.iter().filter_map(|span| span.style.fg).collect()
}

#[test]
fn code_theme_colors_code_blocks() {
    let mut default = App::new("model", status(), Theme::new(true));
    let mut solarized = App::new("model", status(), Theme::new(true));
    solarized.set_code_theme(code_theme("solarized-dark").unwrap());
    let colors = code_colors(&mut solarized);
    assert!(!colors.is_empty());
    assert_ne!(code_colors(&mut default), colors);
}

#[test]
fn code_theme_from_a_textmate_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("red.tmTheme");
    std::fs::write(
        &path,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>name</key><string>Red</string>
<key>settings</key><array><dict><key>settings</key><dict>
<key>foreground</key><string>#FF0000</string>
</dict></dict></array>
</dict></plist>"#,
    )
    .unwrap();

    let mut app = App::new("model", status(), Theme::new(true));
    app.set_code_theme(code_theme(path.to_str().unwrap()).unwrap());
    let colors = code_colors(&mut app);
    assert!(colors.contains(&Color::Rgb(255, 0, 0)), "{colors:?}");
}

#[test]
fn code_theme_names() {
    // Every name the error offers is a theme.
    let err = code_theme("nope").err().unwrap().to_string();
    let names = err
        .split_once("not one of ")
        .and_then(|(_, rest)| rest.split_once(", nor"))
        .map(|(names, _)| names)
        .expect(&err);
    assert_eq!(names.split(", ").count(), 7, "{err}");
    for name in names.split(", ") {
        code_theme(name).unwrap();
    }
    assert!(code_theme("/no/such/theme.tmTheme").is_err());
}

#[test]
fn no_color_keeps_code_plain_with_any_theme() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.set_code_theme(code_theme("solarized-dark").unwrap());
    assert!(code_colors(&mut app).is_empty());
}

#[test]
fn status_reports_model_session_context_and_cache_hit_rate() {
    let mut app = App::new("claude-sonnet-4-5", status(), Theme::new(false));
    assert_eq!(
        app.status_text(None, 0),
        "model:   claude-sonnet-4-5 (default)\nsession: off · 0 messages\ntokens:  0"
    );

    app.apply_update(AgentUpdate::TurnEnd(TokenUsage {
        input: 2_000,
        output: 500,
        cached: 0,
    }));
    app.apply_update(AgentUpdate::TurnEnd(TokenUsage {
        input: 1_000,
        output: 600,
        cached: 7_000,
    }));
    assert_eq!(
        app.status_text(Some("20261122-171500"), 4),
        "model:   claude-sonnet-4-5 (default)\n\
         session: 20261122-171500 · 4 messages\n\
         context: 8K tokens\n\
         tokens:  3K input · 7K cached (70% hit) · 1.1K output"
    );

    // Restoring and resetting usage clears cumulative counts.
    app.reset_session();
    assert!(app.usage().is_empty());
    assert_eq!(
        app.status_text(Some("new"), 1),
        "model:   claude-sonnet-4-5 (default)\nsession: new · 1 message\ntokens:  0"
    );
}

#[test]
fn grep_tool_result_shows_match_count_and_truncation() {
    let mut app = App::new("model", status(), Theme::new(false));
    let mut run_grep = |pattern: &str, content: &str| -> Vec<String> {
        app.apply_update(AgentUpdate::ToolCall {
            name: "grep".into(),
            args: format!(r#"{{"pattern":"{pattern}"}}"#),
        });
        app.apply_update(AgentUpdate::ToolResult {
            content: content.into(),
            details: None,
        });
        app.take_scrollback()
            .into_iter()
            .map(|l| l.to_string())
            .collect()
    };

    let lines = run_grep(
        "foo",
        r#"{"output":"a.rs:1: foo\nb.rs:2: foo\n","matches":2}"#,
    );
    assert!(lines.iter().any(|l| l == "  2 matches"), "{lines:?}");

    let lines = run_grep(
        "bar",
        r#"{"output":"a.rs:1: bar\n","matches":50,"truncated":true}"#,
    );
    assert!(
        lines.iter().any(|l| l == "  50 matches (truncated)"),
        "{lines:?}"
    );
}

#[test]
fn auto_compact_updates_scrollback_and_resets_context() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.apply_update(AgentUpdate::TurnEnd(TokenUsage {
        input: 105_000,
        output: 500,
        cached: 0,
    }));
    assert_eq!(app.context(), 105_000);

    app.apply_update(AgentUpdate::Compacting(105_000));
    app.apply_update(AgentUpdate::Compacted(12));

    assert_eq!(app.context(), 0);
    let lines: Vec<String> = app
        .take_scrollback()
        .into_iter()
        .map(|l| l.to_string())
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l == "[auto-compacting (105K tokens)…]"),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "[compacted 12 messages]"),
        "{lines:?}"
    );

    // Failure keeps the reported context size and shows the error notice.
    app.apply_update(AgentUpdate::TurnEnd(TokenUsage {
        input: 110_000,
        output: 200,
        cached: 0,
    }));
    app.apply_update(AgentUpdate::Compacting(110_000));
    app.apply_update(AgentUpdate::CompactFailed("empty summary".into()));
    assert_eq!(app.context(), 110_000);
    let lines: Vec<String> = app
        .take_scrollback()
        .into_iter()
        .map(|l| l.to_string())
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l == "[auto-compact failed: empty summary]"),
        "{lines:?}"
    );
}

#[test]
fn markdown_tables_scale_to_terminal_width_with_multiline_cells() {
    let table = "\
| Column A | Column B | Description |
| --- | --- | --- |
| short | value | A very long cell description that should wrap onto multiple lines inside the table cell |
| row2 | other | Another long cell description to check row dividers and terminal width scaling |";

    let mut app = App::new("model", status(), Theme::new(false));
    app.set_width(50);
    app.set_code_theme(code_theme("solarized-dark").unwrap());
    app.start_prompt("show table");
    let _ = app.take_scrollback();

    // Stream the table without a trailing blank line, then resize narrower before finishing.
    app.apply_update(AgentUpdate::TextDelta(table.into()));
    assert_eq!(app.handle_event(Event::Resize(44, 20)), Action::Redraw);
    app.apply_update(AgentUpdate::Text(table.into()));
    app.finish_prompt(Ok(()));

    let lines = app.take_scrollback();
    let table_lines: Vec<String> = lines
        .iter()
        .map(ToString::to_string)
        .filter(|l| !l.is_empty())
        .collect();
    assert!(
        table_lines.len() > 6,
        "expected wrapped multiline table rows: {table_lines:#?}"
    );
    for line in &lines {
        assert!(
            line.width() <= 44,
            "table line exceeded terminal width 44 ({}): {:?}",
            line.width(),
            line.to_string()
        );
    }
    // Row dividers separate wrapped body rows cleanly.
    let divider_count = table_lines.iter().filter(|l| l.starts_with('├')).count();
    assert_eq!(divider_count, 2, "{table_lines:#?}");

    // Replayed history also respects the configured terminal width.
    let mut replayed = App::new("model", status(), Theme::new(false));
    replayed.set_width(48);
    replayed.replay_history(&[ChatMessage::assistant(table)]);
    let replayed_lines = replayed.take_scrollback();
    assert!(!replayed_lines.is_empty());
    for line in &replayed_lines {
        assert!(
            line.width() <= 48,
            "replayed table line exceeded width 48 ({}): {:?}",
            line.width(),
            line.to_string()
        );
    }
}

fn scrollback_lines(app: &mut App) -> Vec<String> {
    app.take_scrollback()
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn run_tool(app: &mut App, name: &str, args: &str, content: &str, details: Option<ToolDetails>) {
    app.apply_update(AgentUpdate::ToolCall {
        name: name.into(),
        args: args.into(),
    });
    app.apply_update(AgentUpdate::ToolResult {
        content: content.into(),
        details,
    });
}

#[test]
fn edit_diff_shows_line_numbers_of_each_hunk() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("fix it");
    app.take_scrollback();

    let diff = Diff {
        hunks: vec![
            DiffHunk {
                old_start: 8,
                old_lines: 2,
                new_start: 8,
                new_lines: 2,
                lines: vec![
                    DiffLine::Context("keep".into()),
                    DiffLine::Removed("old".into()),
                    DiffLine::Added("new".into()),
                ],
            },
            DiffHunk {
                old_start: 120,
                old_lines: 1,
                new_start: 120,
                new_lines: 2,
                lines: vec![
                    DiffLine::Context("tail".into()),
                    DiffLine::Added("more".into()),
                ],
            },
        ],
        first_changed_line: Some(9),
        truncated: false,
    };
    let args = r#"{"path":"a.rs","edits":[]}"#;
    run_tool(
        &mut app,
        "edit",
        args,
        r#"{"replacements":2}"#,
        Some(ToolDetails::Edit(diff)),
    );

    assert_eq!(
        scrollback_lines(&mut app),
        [
            "edit a.rs",
            "    8  keep",
            "    9 -old",
            "    9 +new",
            "    ⋮",
            "  120  tail",
            "  121 +more",
            "",
        ]
    );
}

#[test]
fn output_command_shows_the_whole_last_tool_output() {
    let mut app = App::new("model", status(), Theme::new(false));

    app.show_tool_output();
    assert_eq!(scrollback_lines(&mut app), ["[no tool output]"]);

    app.start_prompt("run it");
    app.take_scrollback();
    let output: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
    let content = serde_json::json!({ "exit_code": 0, "output": output.join("\n") });
    run_tool(
        &mut app,
        "bash",
        r#"{"command":"seq"}"#,
        &content.to_string(),
        None,
    );
    let preview = scrollback_lines(&mut app);
    assert!(
        preview.contains(&"  … +12 more lines".to_owned()),
        "{preview:#?}"
    );
    assert!(!preview.contains(&"  line 20".to_owned()), "{preview:#?}");

    app.show_tool_output();
    let full = scrollback_lines(&mut app);
    assert_eq!(full[0], "bash seq");
    let expected: Vec<String> = output.iter().map(|l| format!("  {l}")).collect();
    assert_eq!(full[1..21], expected[..]);

    // Lists show one item per line; other results show their JSON.
    run_tool(
        &mut app,
        "ls",
        "{}",
        r#"{"entries":["a/","b"],"total":2}"#,
        None,
    );
    app.take_scrollback();
    app.show_tool_output();
    assert_eq!(scrollback_lines(&mut app), ["ls .", "  a/", "  b", ""]);

    run_tool(
        &mut app,
        "read",
        r#"{"path":"x"}"#,
        r#"{"error":"no such file"}"#,
        None,
    );
    app.take_scrollback();
    app.show_tool_output();
    assert_eq!(scrollback_lines(&mut app), ["read x", "  no such file", ""]);

    // A long edit diff is shown whole.
    let lines = (0..30).map(|i| DiffLine::Added(format!("l{i}"))).collect();
    let diff = Diff {
        hunks: vec![DiffHunk {
            old_start: 0,
            old_lines: 0,
            new_start: 1,
            new_lines: 30,
            lines,
        }],
        first_changed_line: Some(1),
        truncated: false,
    };
    let args = r#"{"path":"a.rs","edits":[]}"#;
    run_tool(
        &mut app,
        "edit",
        args,
        r#"{"replacements":1}"#,
        Some(ToolDetails::Edit(diff)),
    );
    let preview = scrollback_lines(&mut app);
    assert!(
        preview.contains(&"  … +6 more diff lines".to_owned()),
        "{preview:#?}"
    );
    app.show_tool_output();
    let full = scrollback_lines(&mut app);
    assert_eq!(full.len(), 32, "{full:#?}");
    assert_eq!(full[30], "  30 +l29");

    // A new session forgets it.
    app.reset_session();
    app.show_tool_output();
    assert_eq!(scrollback_lines(&mut app), ["[no tool output]"]);
}

#[test]
fn output_command_shows_a_restored_tool_output() {
    let mut app = App::new("model", status(), Theme::new(false));
    let history = vec![
        ChatMessage::user("check main"),
        ChatMessage::assistant(MessageContent::from_parts(vec![ContentPart::ToolCall(
            ToolCall {
                call_id: "c1".into(),
                fn_name: "read".into(),
                fn_arguments: serde_json::json!({ "path": "src/main.rs" }),
                thought_signatures: None,
            },
        )])),
        ChatMessage::from(ToolResponse::new(
            "c1",
            r#"{"content":"fn main() {}\n","total_lines":1}"#,
        )),
    ];
    app.replay_history(&history);
    app.take_scrollback();

    app.show_tool_output();
    assert_eq!(
        scrollback_lines(&mut app),
        ["read src/main.rs", "  fn main() {}", ""]
    );

    // Replaying a history without tools forgets the previous output.
    app.replay_history(&[ChatMessage::user("hi")]);
    app.take_scrollback();
    app.show_tool_output();
    assert_eq!(scrollback_lines(&mut app), ["[no tool output]"]);
}

#[test]
fn tool_output_keeps_tab_indentation() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("run it");
    app.take_scrollback();

    let content = serde_json::json!({ "exit_code": 0, "output": "a\tb\n\tc" });
    run_tool(
        &mut app,
        "bash",
        r#"{"command":"x"}"#,
        &content.to_string(),
        None,
    );
    assert_eq!(
        scrollback_lines(&mut app),
        ["bash x", "  a   b", "      c", ""]
    );

    let diff = Diff {
        hunks: vec![DiffHunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            lines: vec![
                DiffLine::Removed("\treturn 0;".into()),
                DiffLine::Added("\treturn 1;".into()),
            ],
        }],
        first_changed_line: Some(1),
        truncated: false,
    };
    let args = r#"{"path":"a.c","edits":[]}"#;
    run_tool(
        &mut app,
        "edit",
        args,
        r#"{"replacements":1}"#,
        Some(ToolDetails::Edit(diff)),
    );
    assert_eq!(
        scrollback_lines(&mut app),
        ["edit a.c", "  1 -    return 0;", "  1 +    return 1;", ""]
    );
}

#[test]
fn messages_typed_while_busy_are_queued_and_shown_when_they_join() {
    let mut app = App::new("model", status(), Theme::new(false));
    let mut terminal = inline_terminal(60, 20);
    app.start_prompt("work");
    let _ = app.take_scrollback();

    // Commands wait for the prompt to end, keeping their text.
    type_str(&mut app, "/status");
    assert_eq!(app.handle_event(key(KeyCode::Enter)), Action::None);
    app.handle_event(ctrl('u'));

    type_str(&mut app, "use tabs");
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Submit("use tabs".into())
    );
    terminal.draw(|frame| app.render(frame)).unwrap();
    assert!(render_text(&terminal).contains("1 queued"));

    app.apply_update(AgentUpdate::Steered("use tabs".into()));
    let rows: Vec<String> = app
        .take_scrollback()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(rows.contains(&"> use tabs".to_owned()), "{rows:?}");
    terminal.draw(|frame| app.render(frame)).unwrap();
    assert!(!render_text(&terminal).contains("queued"));
}

#[test]
fn queued_messages_of_a_cancelled_prompt_go_back_to_the_editor() {
    let mut app = App::new("model", status(), Theme::new(false));
    app.start_prompt("work");
    type_str(&mut app, "typed");
    app.cancel_prompt();

    app.restore_queued("queued");

    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Submit("queued\n\ntyped".into())
    );
}

#[test]
fn bang_runs_a_shell_command_and_waits_while_busy() {
    let mut app = App::new("model", status(), Theme::new(false));
    type_str(&mut app, "! git status");
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Shell("git status".into())
    );

    app.start_prompt("work");
    type_str(&mut app, "!ls");
    assert_eq!(app.handle_event(key(KeyCode::Enter)), Action::None);
}
