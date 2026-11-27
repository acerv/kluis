use harness_tui::{AgentUpdate, App, Status, Theme};
use ratatui::backend::TestBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

fn run(app: &mut App, name: &str, args: &str, content: &str) {
    app.apply_update(AgentUpdate::ToolCall {
        name: name.into(),
        args: args.into(),
    });
    app.apply_update(AgentUpdate::ToolResult {
        content: content.into(),
        details: None,
    });
}

fn explore(app: &mut App) {
    run(app, "ls", r#"{"path":"src"}"#, r#"{"entries":["a.rs","b.rs"]}"#);
    run(app, "read", r#"{"path":"src/a.rs"}"#, r#"{"total_lines":120}"#);
    run(app, "read", r#"{"path":"src/b.rs"}"#, r#"{"total_lines":80}"#);
    run(app, "grep", r#"{"pattern":"fn main","path":"src"}"#, r#"{"matches":1}"#);
}

fn print(title: &str, lines: Vec<ratatui::text::Line<'static>>) {
    println!("──── {title} ────");
    for line in lines {
        println!("{line}");
    }
}

fn viewport(app: &App) {
    let height = app.viewport_height(70);
    let mut term = Terminal::with_options(
        TestBackend::new(70, height),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .unwrap();
    term.draw(|f| app.render(f)).unwrap();
    println!("──── viewport while the group is open ────");
    let buf = term.backend().buffer();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        println!("{}", row.trim_end());
    }
}

fn main() {
    let status = Status {
        sandbox: "sandboxed".into(),
        folder: "~/project".into(),
    };
    let mut app = App::new("model", status, Theme::new(false));

    // Quiet mode.
    app.start_prompt("where is main?");
    print("scrollback: prompt", app.take_scrollback());
    explore(&mut app);
    print("scrollback: after 4 read-only calls (held back)", app.take_scrollback());
    viewport(&app);
    run(&mut app, "read", r#"{"path":"gone.rs"}"#, r#"{"error":"no such file"}"#);
    print("scrollback: a failed read ends the group", app.take_scrollback());
    app.apply_update(AgentUpdate::Text("main is in src/b.rs.".into()));
    app.finish_prompt(Ok(()));
    print("scrollback: end of the prompt", app.take_scrollback());

    // Verbose mode.
    app.handle_event(crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('o'),
        crossterm::event::KeyModifiers::CONTROL,
    )));
    app.start_prompt("again, verbose");
    app.take_scrollback();
    explore(&mut app);
    print("scrollback: verbose, each call", app.take_scrollback());
}
