// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Clipboard copy for Linux terminals (OSC 52 + optional local Wayland/X11 helper).

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use genai::chat::{ChatMessage, ChatRole};

const HELPER_TIMEOUT: Duration = Duration::from_millis(500);
const HELPER_POLL: Duration = Duration::from_millis(10);

/// Return the text of the most recent assistant message containing non-empty text.
///
/// Tool-only or reasoning-only assistant turns are skipped.
pub fn last_assistant_text(messages: &[ChatMessage]) -> Option<String> {
    messages.iter().rev().find_map(|msg| {
        if msg.role != ChatRole::Assistant {
            return None;
        }
        msg.content
            .joined_texts()
            .filter(|text| !text.trim().is_empty())
    })
}

/// Format an OSC 52 escape sequence that copies `text` to the terminal's
/// system clipboard (`c`).
pub fn osc52_sequence(text: &str) -> String {
    let encoded = STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

/// Copy `text` to the clipboard using OSC 52 (which works inside the sandbox
/// and over SSH) plus a best-effort local Wayland/X11 tool when its display
/// socket is reachable.
pub fn copy(text: &str) -> io::Result<()> {
    let seq = osc52_sequence(text);
    {
        let mut stdout = io::stdout().lock();
        stdout.write_all(seq.as_bytes())?;
        stdout.flush()?;
    }
    try_native_clipboard(text);
    Ok(())
}

fn try_native_clipboard(text: &str) {
    if wayland_socket().is_some_and(|p| p.exists()) && run_helper("wl-copy", &[], text) {
        return;
    }
    if x11_socket().is_some_and(|p| p.exists()) {
        let _ = run_helper("xclip", &["-selection", "clipboard"], text)
            || run_helper("xsel", &["--clipboard", "--input"], text);
    }
}

fn wayland_socket() -> Option<PathBuf> {
    let display = std::env::var_os("WAYLAND_DISPLAY")?;
    if display.is_empty() {
        return None;
    }
    let path = PathBuf::from(&display);
    if path.is_absolute() {
        return Some(path);
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join(path))
}

fn x11_socket() -> Option<PathBuf> {
    let display = std::env::var("DISPLAY").ok()?;
    let rest = display
        .strip_prefix("unix:")
        .unwrap_or(&display)
        .strip_prefix(':')?;
    let num = rest.split('.').next()?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(PathBuf::from(format!("/tmp/.X11-unix/X{num}")))
}

fn run_helper(program: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let write_ok = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    if !write_ok {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(HELPER_POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}
