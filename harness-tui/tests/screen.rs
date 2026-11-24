// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::fs::File;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use common::{openpty, resize, spawn_tui, spawn_tui_in, spawn_tui_with};
use genai::chat::ChatMessage;
use harness_core::SessionStore;

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

    fn type_text(&mut self, text: &str) {
        self.input.write_all(text.as_bytes()).unwrap();
    }

    fn resize(&mut self, master: &OwnedFd, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(nz(rows), nz(cols));
        resize(master, rows, cols);
    }
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

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    child.wait().unwrap();
}

#[test]
fn narrower_terminal_rewraps_output() {
    const PROMPT: &str = "Type a message";
    // The end of a `/help` line that a 40 columns terminal cuts.
    const TAIL: &str = "low ... max)";
    let home = tempfile::tempdir().unwrap();
    let (master, slave) = openpty(60, 80);
    let mut child = spawn_tui(home.path(), &slave);
    let mut term = Emulator::new(&master, 60, 80);

    term.wait_for(PROMPT);
    term.type_text("/help\r");
    term.wait_for("Ctrl-D");
    term.resize(&master, 60, 40);
    // The emulator cuts lines instead of rewrapping them: the tail shows only
    // if the output is printed again at the new width.
    term.wait_for(TAIL);
    let screen = term.rows().join("\n");
    assert_eq!(screen.matches("Ctrl-D").count(), 1, "{screen}");
    let rule = term.row_of("────").unwrap();
    assert!(term.row_of("Ctrl-D").unwrap() < rule, "{screen}");
    assert!(rule < term.row_of(PROMPT).unwrap(), "{screen}");

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
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

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
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

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
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

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
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

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    child.wait().unwrap();
}
