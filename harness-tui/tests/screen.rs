// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::fs::File;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use common::{openpty, resize, spawn_tui};

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
