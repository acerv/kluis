// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Clipboard for Linux terminals: copy with OSC 52 and an optional local
//! Wayland/X11 helper, paste images with that helper.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use genai::chat::{ChatMessage, ChatRole};
use harness_core::tools::{Image, MAX_IMAGE_BYTES};

const HELPER_TIMEOUT: Duration = Duration::from_millis(500);
/// Longest wait for a helper to hand over an image of the clipboard.
const PASTE_TIMEOUT: Duration = Duration::from_secs(2);
/// Most bytes read of the list of types of the clipboard.
const MAX_TYPES_BYTES: u64 = 64 * 1024;
/// The image types the providers take, preferred first.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
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

/// The image in the clipboard, read with the local Wayland/X11 helper.
pub fn paste_image() -> Result<Image, String> {
    let bytes = native_image().ok_or("no image in the clipboard")?;
    Image::from_bytes(&bytes)
}

fn native_image() -> Option<Vec<u8>> {
    // Up to one byte more than an image may hold, to tell it is too large.
    let max = MAX_IMAGE_BYTES + 1;
    if wayland_socket().is_some_and(|p| p.exists()) {
        let types = read_helper("wl-paste", &["--list-types"], MAX_TYPES_BYTES)?;
        let mime = image_type(&types)?;
        return read_helper("wl-paste", &["--no-newline", "--type", mime], max);
    }
    if x11_socket().is_some_and(|p| p.exists()) {
        let xclip =
            |mime, max| read_helper("xclip", &["-selection", "clipboard", "-o", "-t", mime], max);
        let types = xclip("TARGETS", MAX_TYPES_BYTES)?;
        return xclip(image_type(&types)?, max);
    }
    None
}

/// The image type the providers take among the `types` of the clipboard,
/// one per line.
fn image_type(types: &[u8]) -> Option<&'static str> {
    let types = String::from_utf8_lossy(types);
    IMAGE_TYPES
        .into_iter()
        .find(|mime| types.lines().any(|line| line.trim() == *mime))
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
    let Some(mut stdin) = child.stdin.take() else {
        return wait_helper(&mut child, Duration::ZERO);
    };
    // Written apart, for the wait to stop a helper that does not read: a
    // text longer than the pipe holds would wait for it forever.
    thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(text.as_bytes()).is_ok());
        let ok = wait_helper(&mut child, HELPER_TIMEOUT);
        // The helper is gone, and what it did not read with it.
        writer.join().unwrap_or(false) && ok
    })
}

/// Up to `max` bytes of what `program` writes, when it succeeds in time.
fn read_helper(program: &str, args: &[&str], max: u64) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read apart, for the wait to stop a helper that hangs. What comes
    // past `max` is read and dropped, for the helper to end.
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        (&mut stdout).take(max).read_to_end(&mut bytes)?;
        io::copy(&mut stdout, &mut io::sink())?;
        Ok::<_, io::Error>(bytes)
    });
    let ok = wait_helper(&mut child, PASTE_TIMEOUT);
    // The helper is gone, and the end of its output with it.
    let bytes = reader.join().ok()?.ok()?;
    ok.then_some(bytes)
}

/// Whether `child` succeeds within `timeout`; it is killed otherwise.
fn wait_helper(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
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
