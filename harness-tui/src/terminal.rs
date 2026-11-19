// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::thread;
use std::time::Duration;

use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, Clear, ClearType,
    EndSynchronizedUpdate,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use scopeguard::ScopeGuard;
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;

const EVENT_POLL: Duration = Duration::from_millis(10);

static PANIC_HOOK: Once = Once::new();

/// Reads crossterm events on a background thread with a short poll timeout so
/// `crossterm::cursor::position()` (called by ratatui's inline viewport on
/// resize) can acquire crossterm's internal reader lock promptly instead of
/// deadlocking against `EventStream`'s infinite poll.
pub struct EventReader {
    rx: mpsc::UnboundedReceiver<io::Result<Event>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl EventReader {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match crossterm::event::poll(EVENT_POLL) {
                    Ok(true) => match crossterm::event::read() {
                        Ok(ev) => {
                            if tx.send(Ok(ev)).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err));
                            break;
                        }
                    },
                    Ok(false) => {}
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        break;
                    }
                }
            }
        });
        Self {
            rx,
            stop,
            handle: Some(handle),
        }
    }

    pub async fn next(&mut self) -> Option<io::Result<Event>> {
        self.rx.recv().await
    }

    pub fn try_next(&mut self) -> Option<io::Result<Event>> {
        self.rx.try_recv().ok()
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Owns the inline ratatui terminal and restores raw mode and bracketed paste
/// on drop or panic.
pub struct InlineTerminal {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    height: u16,
    pending_clear: bool,
}

impl InlineTerminal {
    pub fn enter(height: u16) -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(err);
        }
        let backend = CrosstermBackend::new(stdout);
        let options = TerminalOptions {
            viewport: Viewport::Inline(height),
        };
        match Terminal::with_options(backend, options) {
            Ok(mut terminal) => {
                let height = terminal.get_frame().area().height;
                Ok(Self {
                    terminal,
                    height,
                    pending_clear: false,
                })
            }
            Err(err) => {
                restore_terminal();
                Err(err)
            }
        }
    }

    /// Schedule a full screen and scrollback clear on the next [`draw_app`](Self::draw_app),
    /// executed atomically inside its synchronized update.
    pub fn request_clear(&mut self) {
        self.pending_clear = true;
    }

    /// Sync terminal dimensions, resize the inline viewport if needed, flush
    /// pending scrollback lines above it, and redraw `app` inside a single
    /// synchronized terminal update.
    pub fn draw_app(&mut self, app: &mut App) -> io::Result<()> {
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        let sync = scopeguard::guard((), |()| {
            let _ = execute!(io::stdout(), EndSynchronizedUpdate);
        });
        let res = (|| -> io::Result<()> {
            // Sync ratatui's viewport before querying its top row or calling
            // `insert_before` (ratatui otherwise only autoresizes inside `draw`).
            self.terminal.autoresize()?;

            let size = self.terminal.size()?;
            let desired = app.viewport_height(size.width).min(size.height.max(1));
            if self.pending_clear {
                self.pending_clear = false;
                execute!(
                    io::stdout(),
                    Clear(ClearType::Purge),
                    Clear(ClearType::All),
                    MoveTo(0, 0),
                )?;
                self.recreate_viewport(desired)?;
            } else if desired != self.height {
                let top = self.terminal.get_frame().area().y;
                execute!(
                    io::stdout(),
                    MoveTo(0, top),
                    Clear(ClearType::FromCursorDown)
                )?;
                self.recreate_viewport(desired)?;
            }

            let lines = app.take_scrollback();
            flush_scrollback(&mut self.terminal, lines)?;
            self.terminal.draw(|frame| app.render(frame))?;
            Ok(())
        })();
        ScopeGuard::into_inner(sync);
        let end_res = execute!(io::stdout(), EndSynchronizedUpdate);
        res.and(end_res)
    }

    fn recreate_viewport(&mut self, height: u16) -> io::Result<()> {
        let backend = CrosstermBackend::new(io::stdout());
        let options = TerminalOptions {
            viewport: Viewport::Inline(height),
        };
        self.terminal = Terminal::with_options(backend, options)?;
        self.height = self.terminal.get_frame().area().height;
        Ok(())
    }
}

impl Drop for InlineTerminal {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Write `lines` into the terminal's native scrollback above the inline
/// viewport, wrapping to the current terminal width.
pub fn flush_scrollback<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
) -> Result<(), B::Error> {
    if lines.is_empty() {
        return Ok(());
    }
    let width = terminal.get_frame().area().width as usize;
    if width == 0 {
        return Ok(());
    }
    let wrapped = wrap_lines(&lines, width);
    let height = wrapped.len().min(u16::MAX as usize) as u16;
    if height == 0 {
        return Ok(());
    }
    let paragraph = Paragraph::new(wrapped);
    terminal.insert_before(height, |buf| {
        paragraph.render(buf.area, buf);
    })
}

/// Display width of a single Unicode character in terminal cells.
pub(crate) fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// Truncate `text` to fit within `max_width` terminal cells, appending `…` if cut.
pub(crate) fn truncate_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.width() <= max_width {
        return text.to_owned();
    }
    let ellipsis = '…';
    let ellipsis_w = char_width(ellipsis);
    let budget = max_width.saturating_sub(ellipsis_w);
    let mut out = String::new();
    let mut col = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if col + w > budget {
            break;
        }
        out.push(ch);
        col += w;
    }
    if ellipsis_w <= max_width {
        out.push(ellipsis);
    }
    out
}

/// Count how many rows `lines` occupy when soft-wrapped to `width` cells,
/// without allocating wrapped lines.
pub(crate) fn wrapped_line_count(lines: &[Line<'static>], width: usize) -> usize {
    let width = width.max(1);
    let mut total = 0usize;
    for line in lines {
        if line.width() <= width {
            total += 1;
            continue;
        }
        let mut col = 0usize;
        let mut rows = 1usize;
        for span in &line.spans {
            for ch in span.content.chars() {
                let ch_w = char_width(ch);
                if col > 0 && col + ch_w > width {
                    rows += 1;
                    col = 0;
                }
                col += ch_w;
            }
        }
        total += rows;
    }
    total
}

/// Wrap styled lines so each resulting line fits within `width` cells.
pub(crate) fn wrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= width {
            out.push(line.clone());
            continue;
        }
        let line_style = line.style;
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize;
        for span in &line.spans {
            let style = line_style.patch(span.style);
            let mut chunk = String::new();
            for ch in span.content.chars() {
                let ch_w = char_width(ch);
                if col > 0 && col + ch_w > width {
                    if !chunk.is_empty() {
                        current.push(Span::styled(std::mem::take(&mut chunk), style));
                    }
                    out.push(Line::from(std::mem::take(&mut current)));
                    col = 0;
                }
                chunk.push(ch);
                col += ch_w;
            }
            if !chunk.is_empty() {
                current.push(Span::styled(chunk, style));
            }
        }
        if !current.is_empty() {
            out.push(Line::from(current));
        }
    }
    out
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            prev(info);
        }));
    });
}

fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, EndSynchronizedUpdate, DisableBracketedPaste, Show);
    let _ = disable_raw_mode();
    let _ = writeln!(stdout);
    let _ = stdout.flush();
}
