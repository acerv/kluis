// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use genai::chat::{ChatMessage, ContentPart, MessageContent, ToolCall, ToolResponse};
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

#[test]
fn footer_shows_sandbox_folder_model_and_scaled_tokens() {
    let mut term = inline_terminal(80, 14);
    let mut app = App::new("deepseek-chat", status(), Theme::new(true));

    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("sandboxed · ~/project"), "{screen}");
    assert!(screen.contains("deepseek-chat (default) · 0"), "{screen}");
    assert!(screen.contains("────────"), "{screen}");

    // 1,200 + 300 = 1,500 -> 1.5K
    app.apply_update(AgentUpdate::TurnEnd {
        input_tokens: 1_200,
        output_tokens: 300,
    });
    app.set_model("gpt-4o");
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    let left_pos = screen
        .find("gpt-4o (default) · 1.5K")
        .expect("model (thinking) · tokens in footer");
    let right_pos = screen
        .find("sandboxed · ~/project")
        .expect("sandbox · folder in footer");
    assert!(left_pos < right_pos, "{screen}");

    // +2,350,000 -> 2,351,500 -> 2.3M
    app.apply_update(AgentUpdate::TurnEnd {
        input_tokens: 2_000_000,
        output_tokens: 350_000,
    });
    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("gpt-4o (default) · 2.3M"), "{screen}");
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

#[test]
fn replay_history_and_restore_tokens() {
    let mut term = inline_terminal(80, 14);
    let mut app = App::new("deepseek-chat", status(), Theme::new(true));
    app.set_tokens(1_200, 300);

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

    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(
        screen.contains("deepseek-chat (default) · 1.5K"),
        "{screen}"
    );
}

#[test]
fn session_picker_navigation_filtering_and_resume_slash() {
    use harness_core::SessionSummary;
    use std::path::PathBuf;

    let type_str = |app: &mut App, s: &str| {
        for ch in s.chars() {
            app.handle_event(key(KeyCode::Char(ch)));
        }
    };

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

    // Shift+Tab cycles default -> low -> medium -> high -> default -> low and updates footer.
    for expected in [
        Thinking::Low,
        Thinking::Medium,
        Thinking::High,
        Thinking::Default,
        Thinking::Low,
    ] {
        assert_eq!(
            app.handle_event(shift(KeyCode::BackTab)),
            Action::SetThinking(expected)
        );
        assert_eq!(app.thinking(), expected);
    }

    term.draw(|f| app.render(f)).unwrap();
    let screen = render_text(&term);
    assert!(screen.contains("claude-sonnet-4-5 (low) · 0"), "{screen}");
    assert!(screen.contains("Thinking: low"), "{screen}");

    // Footer model color changes by thinking effort when color is enabled.
    let mut colored = App::new("deepseek-flash", status(), Theme::new(true));
    for (level, expected_fg) in [
        (Thinking::Default, Color::Green),
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
    assert!(screen.contains("/new"), "{screen}");
    assert!(screen.contains("/quit"), "{screen}");
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
    assert!(screen.contains("/thinking high"), "{screen}");

    // Down selects `low`, Enter submits `/thinking low` and closes popup.
    assert_eq!(app.handle_event(key(KeyCode::Down)), Action::Redraw);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Action::Slash(SlashCommand::Thinking(Some("low".into())))
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
