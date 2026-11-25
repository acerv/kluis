// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::fs::File;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use common::{
    delta, kill, line_count, openpty, read_request, resize, spawn_sandboxed_tui_in, spawn_tui,
    spawn_tui_in, spawn_tui_with, tempdir, wait_until, write_sse,
};
use genai::chat::ChatMessage;
use harness_core::SessionStore;
use serde_json::json;

/// Cursor position query sent by crossterm.
const DSR: &[u8] = b"\x1b[6n";

/// A minimal terminal emulator on the master side of a pseudo-terminal.
struct Emulator {
    parser: vt100::Parser,
    output: Receiver<Vec<u8>>,
    input: File,
    pending: Vec<u8>,
}

impl Emulator {
    fn new(master: &OwnedFd, rows: u16, cols: u16) -> Self {
        let mut reader = File::from(master.try_clone().unwrap());
        let (tx, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            parser: vt100::Parser::new(nz(rows), nz(cols), 0),
            output,
            input: File::from(master.try_clone().unwrap()),
            pending: Vec::new(),
        }
    }

    /// Show `bytes`, answering cursor position queries as a terminal does.
    fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        while let Some(at) = self.pending.windows(DSR.len()).position(|w| w == DSR) {
            self.parser.process(&self.pending[..at]);
            self.pending.drain(..at + DSR.len());
            let (row, col) = self.parser.screen().cursor_position();
            let reply = format!("\x1b[{};{}R", row + 1, col + 1);
            self.input.write_all(reply.as_bytes()).unwrap();
        }
        // Keep a possible start of a query for the next read.
        let keep = (1..DSR.len())
            .rev()
            .find(|&n| self.pending.ends_with(&DSR[..n]))
            .unwrap_or(0);
        let done = self.pending.len() - keep;
        self.parser.process(&self.pending[..done]);
        self.pending.drain(..done);
    }

    fn rows(&self) -> Vec<String> {
        let (_, cols) = self.parser.screen().size();
        self.parser.screen().rows(0, cols.get()).collect()
    }

    fn row_of(&self, text: &str) -> Option<usize> {
        self.rows().iter().position(|r| r.contains(text))
    }

    /// Show output until `text` is on screen and nothing more arrives.
    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.output.recv_timeout(Duration::from_millis(300)) {
                Ok(bytes) => self.feed(&bytes),
                Err(RecvTimeoutError::Timeout) if self.row_of(text).is_some() => return,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("terminal closed"),
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {text:?}:\n{}",
                self.rows().join("\n")
            );
        }
    }

    /// Show the output that arrived.
    fn feed_output(&mut self) {
        for bytes in self.output.try_iter().collect::<Vec<_>>() {
            self.feed(&bytes);
        }
    }

    /// Show output until `done`, which tells `what` happened.
    fn wait_until(&mut self, what: &str, done: impl Fn() -> bool) {
        wait_until(what, || {
            self.feed_output();
            done()
        });
    }

    /// Show output until `text` is on screen, even if more keeps coming,
    /// as while a prompt runs.
    fn wait_for_busy(&mut self, text: &str) {
        wait_until(text, || {
            self.feed_output();
            self.row_of(text).is_some()
        });
    }

    fn type_text(&mut self, text: &str) {
        self.input.write_all(text.as_bytes()).unwrap();
    }

    fn resize(&mut self, master: &OwnedFd, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(nz(rows), nz(cols));
        resize(master, rows, cols);
    }
}

fn is_raw(fd: &OwnedFd) -> bool {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) }, 0);
    t.c_lflag & (libc::ICANON | libc::ECHO) == 0
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

#[test]
fn resize_keeps_output_above_prompt() {
    const PROMPT: &str = "Type a message";
    // Last line of `/help`, the one right above the prompt.
    const OUTPUT: &str = "Ctrl-D";
    let home = tempfile::tempdir().unwrap();
    let (master, slave) = openpty(24, 80);
    let mut child = spawn_tui(home.path(), &slave);
    let mut term = Emulator::new(&master, 24, 80);

    term.wait_for(PROMPT);
    term.type_text("/help\r");
    term.wait_for(OUTPUT);
    let output = term.row_of(OUTPUT).unwrap();
    let prompt = term.row_of(PROMPT).unwrap();
    assert!(output < prompt, "{}", term.rows().join("\n"));

    for (rows, cols) in [(24, 50), (24, 70), (20, 70)] {
        term.resize(&master, rows, cols);
        // Let the TUI redraw, then check nothing moved.
        std::thread::sleep(Duration::from_millis(200));
        term.wait_for(PROMPT);
        let screen = term.rows().join("\n");
        let (Some(new_output), Some(new_prompt)) = (term.row_of(OUTPUT), term.row_of(PROMPT))
        else {
            panic!("{rows}x{cols}: output or prompt gone:\n{screen}");
        };
        // The prompt stays right below the output it followed.
        assert_eq!(new_prompt - new_output, prompt - output, "{screen}");
    }

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

#[test]
fn narrower_terminal_rewraps_output() {
    const PROMPT: &str = "Type a message";
    // The end of a `/help` line that a 40 columns terminal cuts.
    const TAIL: &str = "low ... max)";
    // Enough for all of `/help` once its lines wrap.
    const ROWS: u16 = 100;
    let home = tempfile::tempdir().unwrap();
    let (master, slave) = openpty(ROWS, 80);
    let mut child = spawn_tui(home.path(), &slave);
    let mut term = Emulator::new(&master, ROWS, 80);

    term.wait_for(PROMPT);
    term.type_text("/help\r");
    term.wait_for("Ctrl-D");
    term.resize(&master, ROWS, 40);
    // The emulator cuts lines instead of rewrapping them: the tail shows only
    // if the output is printed again at the new width.
    term.wait_for(TAIL);
    let screen = term.rows().join("\n");
    assert_eq!(screen.matches("Ctrl-D").count(), 1, "{screen}");
    let rule = term.row_of("────").unwrap();
    assert!(term.row_of("Ctrl-D").unwrap() < rule, "{screen}");
    assert!(rule < term.row_of(PROMPT).unwrap(), "{screen}");

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

#[test]
fn banner_shows_startup_info() {
    let home = tempfile::tempdir().unwrap();
    let (master, slave) = openpty(24, 100);
    let mut child = spawn_tui(home.path(), &slave);
    let mut term = Emulator::new(&master, 24, 100);

    term.wait_for("Type a message");
    let screen = term.rows().join("\n");
    let version = format!("█ ▄▀ █     kluis v{}", env!("CARGO_PKG_VERSION"));
    assert!(screen.contains(&version), "{screen}");
    for text in [
        "█▀▄  █     deepseek · ",
        "▀  ▀ ▀▀▀   no sandbox",
        "           context: ",
        "           new session",
    ] {
        assert!(screen.contains(text), "{text:?} missing:\n{screen}");
    }

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

#[test]
fn session_picked_at_startup_takes_the_model_asked_for() {
    let home = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    let store = SessionStore::new(home.path().join(".kluis/sessions"), &cwd);
    let mut writer = store.create("first-model");
    writer
        .append_messages(&[ChatMessage::user("an old question")])
        .unwrap();

    let (master, slave) = openpty(24, 100);
    let args = ["--resume", "--model", "asked-model"];
    let mut child = spawn_tui_with(home.path(), &slave, &args);
    let mut term = Emulator::new(&master, 24, 100);

    term.wait_for("an old question");
    term.type_text("\r");
    term.wait_for("[resumed session");
    let screen = term.rows().join("\n");
    assert!(screen.contains("asked-model"), "{screen}");
    assert!(!screen.contains("first-model"), "{screen}");
    // As `--resume <id> --model` does, for good.
    assert_eq!(store.latest().unwrap().unwrap().meta.model, "asked-model");

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

#[test]
fn thinking_falls_back_to_a_level_the_model_takes() {
    use std::os::unix::fs::PermissionsExt;

    // An Ollama server whose model takes only on and off thinking.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"thinking":{"values":[true,false],"default":true}}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let home = tempfile::tempdir().unwrap();
    let auth = home.path().join(".kluis/auth.toml");
    std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
    std::fs::write(
        &auth,
        format!("[ollama]\nendpoint = \"http://127.0.0.1:{port}/\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();

    let (master, slave) = openpty(24, 100);
    let args = ["--model", "ollama::qwen3", "--thinking", "high"];
    let mut child = spawn_tui_with(home.path(), &slave, &args);
    let mut term = Emulator::new(&master, 24, 100);

    term.wait_for("[ollama::qwen3 does not take thinking high; using default]");
    term.type_text("/thinking max\r");
    term.wait_for("it takes default, off]");
    term.type_text("/thinking\r");
    term.wait_for("thinking: default (this model takes default, off)");

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

#[test]
fn diff_panel_shows_changes_and_keeps_the_output() {
    const PROMPT: &str = "Type a message";
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("a.txt");
    std::fs::write(&file, "one\n").unwrap();
    let (master, slave) = openpty(24, 100);
    let mut child = spawn_tui_in(home.path(), project.path(), &slave);
    let mut term = Emulator::new(&master, 24, 100);

    term.wait_for(PROMPT);
    term.type_text("/thinking\r");
    term.wait_for("thinking: default");
    term.type_text("/diff\r");
    term.wait_for("No changes since the session started");
    term.type_text("/diff\r");
    term.wait_for(PROMPT);
    std::fs::write(&file, "one\ntwo\n").unwrap();
    term.type_text("/diff\r");
    term.wait_for("+1 -0");
    // Output while the panel is open shows on the left.
    term.type_text("/status\r");
    term.wait_for("session: ");
    let screen = term.rows().join("\n");
    let row = term.row_of("M a.txt").unwrap();
    assert!(term.rows()[row].find("M a.txt").unwrap() > 50, "{screen}");
    assert!(term.rows()[term.row_of("session: ").unwrap()].starts_with("session: "));
    assert!(screen.contains("+two"), "{screen}");

    // Printed above the prompt once the panel closes, then at the new
    // width after a resize.
    let check_output = |term: &mut Emulator| {
        std::thread::sleep(Duration::from_millis(200));
        term.wait_for(PROMPT);
        let screen = term.rows().join("\n");
        assert!(!screen.contains("M a.txt"), "{screen}");
        assert_eq!(screen.matches("thinking: default").count(), 1, "{screen}");
        assert_eq!(screen.matches("session: ").count(), 1, "{screen}");
        let status = term.row_of("session: ").unwrap();
        assert!(
            term.row_of("thinking: default").unwrap() < status,
            "{screen}"
        );
        assert!(status < term.row_of(PROMPT).unwrap(), "{screen}");
    };
    term.type_text("/diff\r");
    check_output(&mut term);
    term.type_text("/diff\r");
    term.wait_for("M a.txt");
    term.resize(&master, 24, 80);
    term.wait_for("M a.txt");
    term.type_text("/diff\r");
    check_output(&mut term);

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

/// A home folder whose config makes the model served at `port` the
/// default one, followed by `config`.
fn home_for_model(port: u16, config: &str) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let kluis = home.path().join(".kluis");
    std::fs::create_dir_all(&kluis).unwrap();
    std::fs::write(
        kluis.join("config.toml"),
        format!("[generic]\nmodel = \"genai_1::test-model\"\n{config}"),
    )
    .unwrap();
    let auth = kluis.join("auth.toml");
    let endpoint = format!("http://127.0.0.1:{port}/v1/");
    std::fs::write(
        &auth,
        format!("[genai_1]\nendpoint = \"{endpoint}\"\napi_key = \"test-key\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    home
}

#[test]
fn tool_call_waits_for_the_approval_of_the_user() {
    // A model that writes a file, then answers "done", in turn.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (turn, stream) in listener.incoming().flatten().enumerate() {
            read_request(&stream);
            let call = json!({
                "index": 0,
                "id": format!("call_{turn}"),
                "type": "function",
                "function": {
                    "name": "write",
                    "arguments": json!({ "path": "new.txt", "content": "hello\n" }).to_string(),
                },
            });
            let chunks = if turn % 2 == 0 {
                [
                    delta(json!({ "tool_calls": [call] }), None),
                    delta(json!({}), Some("tool_calls")),
                ]
            } else {
                [
                    delta(json!({ "content": "done" }), None),
                    delta(json!({}), Some("stop")),
                ]
            };
            write_sse(stream, &chunks);
        }
    });
    let home = home_for_model(port, "[permissions]\nask = [\"write\"]\n");
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("new.txt");

    let (master, slave) = openpty(40, 100);
    let mut child = spawn_tui_in(home.path(), project.path(), &slave);
    let mut term = Emulator::new(&master, 40, 100);
    term.wait_for("Type a message");

    // Typed text refuses the call and tells the model why.
    term.type_text("first\r");
    term.wait_for_busy("Always in this session");
    assert!(term.row_of("Allow write?").is_some(), "{:?}", term.rows());
    term.type_text("not now\r");
    term.wait_for("done");
    assert!(!file.exists());

    term.type_text("second\r");
    term.wait_for_busy("Always in this session");
    term.type_text("\r");
    term.wait_until("the file", || file.exists());

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

#[test]
fn undo_puts_files_back_and_drops_the_prompt() {
    const PROMPT: &str = "Type a message";
    // A model that answers "done" to every request.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            read_request(&stream);
            let chunks = [
                delta(json!({ "content": "done" }), None),
                delta(json!({}), Some("stop")),
            ];
            write_sse(stream, &chunks);
        }
    });
    let home = home_for_model(port, "");
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("a.txt");
    std::fs::write(&file, "one\n").unwrap();

    let (master, slave) = openpty(40, 100);
    let mut child = spawn_tui_in(home.path(), project.path(), &slave);
    let mut term = Emulator::new(&master, 40, 100);
    term.wait_for(PROMPT);
    term.type_text("/undo\r");
    term.wait_for("[nothing to undo]");

    term.type_text("first prompt\r");
    term.wait_for("done");
    term.wait_for(PROMPT);
    // What the tools of the model would do during the second prompt.
    term.type_text("second prompt\r");
    term.wait_until("the second answer", || {
        let store = SessionStore::new(home.path().join(".kluis/sessions"), project.path());
        store
            .latest()
            .ok()
            .flatten()
            .is_some_and(|session| session.messages.len() == 4)
    });
    std::fs::write(&file, "one\ntwo\n").unwrap();
    std::fs::write(project.path().join("new.txt"), "new\n").unwrap();

    // Cancelled first: nothing changes.
    term.type_text("/undo\r");
    term.wait_for("restore a.txt");
    term.wait_for("delete  new.txt");
    term.type_text("\x1b");
    term.wait_for(PROMPT);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\n");

    term.type_text("/undo\r");
    term.wait_for("Put these files back");
    term.type_text("\r");
    term.wait_for("[undone: 2 files put back, last prompt dropped]");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
    assert!(!project.path().join("new.txt").exists());
    let store = SessionStore::new(home.path().join(".kluis/sessions"), project.path());
    let session = store.latest().unwrap().unwrap();
    assert_eq!(session.messages.len(), 2, "{:?}", session.messages);

    // The first prompt changed no file: only the conversation goes back.
    term.type_text("/undo\r");
    term.wait_for("changed no file");
    term.type_text("\r");
    term.wait_for("[undone: 0 files put back, last prompt dropped]");
    term.type_text("/undo\r");
    term.wait_for("[nothing to undo]");

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

#[test]
fn shell_command_ends_on_screen() {
    let home = tempfile::tempdir().unwrap();
    let (master, slave) = openpty(24, 100);
    let mut child = spawn_tui(home.path(), &slave);
    let mut term = Emulator::new(&master, 24, 100);

    term.wait_for("(/help for commands)");
    // Without a sandbox, the command ends as its output comes: the screen
    // must still show the prompt over, without waiting for a key.
    term.type_text("!echo hello\r");
    term.wait_for("  hello");
    term.wait_for("(/help for commands)");
    let screen = term.rows().join("\n");
    assert!(!screen.contains("Running bash"), "{screen}");

    kill(&child, libc::SIGTERM);
    child.wait().unwrap();
}

#[test]
fn signals_restore_terminal() {
    for (signal, name) in [
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGHUP, "SIGHUP"),
        (libc::SIGINT, "SIGINT"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let (master, slave) = openpty(24, 80);
        let mut child = spawn_tui(home.path(), &slave);
        let mut term = Emulator::new(&master, 24, 80);

        term.wait_for("Type a message");
        assert!(is_raw(&slave), "{name}");
        kill(&child, signal);
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(128 + signal), "{name}: {status:?}");
        assert_eq!(status.signal(), None, "{name}");
        assert!(!is_raw(&slave), "{name}: terminal left in raw mode");
    }
}

#[test]
fn signal_stops_the_command_and_checks_git() {
    let home = tempdir();
    let cwd = tempdir();
    let (master, slave) = openpty(24, 80);
    let mut child = spawn_sandboxed_tui_in(home.path(), cwd.path(), &slave);
    let mut term = Emulator::new(&master, 24, 80);

    term.wait_for("Type a message");
    term.type_text("!mkdir .git; while :; do echo tick >> ticks; sleep 0.02; done\r");
    let ticks = cwd.path().join("ticks");
    term.wait_until("the command", || ticks.exists());
    kill(&child, libc::SIGTERM);
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
    // Git on the host would run what the config of a new repository names.
    assert!(cwd.path().join(".git.kl-disabled").is_dir());
    assert!(!cwd.path().join(".git").exists());
    let at_exit = line_count(&ticks);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(line_count(&ticks), at_exit);
}
