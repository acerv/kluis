// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::thread::{self, ThreadId};
use std::time::Duration;

use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::style::{Print, PrintStyledContent, StyledContent};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, Clear, ClearType,
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use crossterm::{execute, queue};
use ratatui::backend::{Backend, CrosstermBackend, IntoCrossterm};
use ratatui::layout::{Constraint, Layout, Size};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use scopeguard::ScopeGuard;
use tokio::sync::mpsc;

use crate::app::{App, MIN_SPLIT_WIDTH};
use crate::text::wrap_lines;
use crate::transcript::{Entry, Transcript};

const EVENT_POLL: Duration = Duration::from_millis(10);

/// Most lines printed again when the terminal width changes.
const MAX_REPRINT_LINES: usize = 10_000;

/// Save the terminal title on the terminal's stack (xterm), to put it back
/// on exit; terminals without the stack ignore it.
const PUSH_TITLE: &str = "\x1b[22;0t";
const POP_TITLE: &str = "\x1b[23;0t";

static PANIC_HOOK: Once = Once::new();

/// The alternate screen is in use, to leave it on exit.
static ALTERNATE: AtomicBool = AtomicBool::new(false);

/// The terminal is in raw mode, to restore it only once.
static ENTERED: AtomicBool = AtomicBool::new(false);

/// The thread that draws the terminal: its panic ends the interface, the
/// one of another thread does not.
static OWNER: Mutex<Option<ThreadId>> = Mutex::new(None);

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
    /// Terminal size at the last draw.
    size: Size,
    /// Row of the terminal cursor after the last draw: the only point that
    /// still tells where the viewport is once the terminal resizes.
    cursor_row: u16,
    pending_clear: bool,
    /// The last entries written above the viewport, to print them again
    /// at a new terminal width.
    transcript: Transcript,
    /// The terminal title set last.
    title: String,
    /// While the `/diff` panel is open, on the alternate screen: the lines
    /// of the transcript not printed yet, and where the viewport was.
    split: Option<Split>,
}

struct Split {
    unprinted: Vec<Line<'static>>,
    top: u16,
    /// The screen was cleared meanwhile.
    cleared: bool,
}

impl InlineTerminal {
    /// Enter raw mode with an inline viewport of `height` rows.
    pub fn enter(height: u16) -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        *OWNER.lock().unwrap_or_else(PoisonError::into_inner) = Some(thread::current().id());
        ENTERED.store(true, Ordering::Relaxed);
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnableBracketedPaste, Print(PUSH_TITLE)) {
            restore_terminal();
            return Err(err);
        }
        let backend = CrosstermBackend::new(stdout);
        let options = TerminalOptions {
            viewport: Viewport::Inline(height),
        };
        match Terminal::with_options(backend, options).and_then(|t| Ok((t.size()?, t))) {
            Ok((size, mut terminal)) => {
                let area = terminal.get_frame().area();
                Ok(Self {
                    terminal,
                    height: area.height,
                    size,
                    cursor_row: area.y,
                    pending_clear: false,
                    transcript: Transcript::default(),
                    title: String::new(),
                    split: None,
                })
            }
            Err(err) => {
                restore_terminal();
                Err(err)
            }
        }
    }

    /// Current terminal width in columns.
    pub fn width(&self) -> u16 {
        self.size.width
    }

    /// Schedule a full screen and scrollback clear on the next [`draw_app`](Self::draw_app),
    /// executed atomically inside its synchronized update.
    pub fn request_clear(&mut self) {
        self.pending_clear = true;
    }

    /// Sync terminal dimensions, resize the inline viewport if needed, flush
    /// pending scrollback lines above it, and redraw `app` inside a single
    /// synchronized terminal update.
    ///
    /// When the terminal width changes, the screen and scrollback are
    /// cleared and the transcript is printed again at the new width: a
    /// terminal that rewraps its text also rewraps the old viewport, in its
    /// own way, leaving pieces of it behind. The same happens when the
    /// output mode changes, to show the transcript in the new mode. When
    /// only the height changes, the viewport follows the cursor, which the
    /// terminal moves along with the text, so the output above stays in
    /// place.
    ///
    /// While the `/diff` panel is open, the whole screen is drawn instead,
    /// on the alternate screen: the last lines of the transcript and the
    /// viewport on the left, the panel on the right. The lines added
    /// meanwhile are printed once the panel closes.
    pub fn draw_app(&mut self, app: &mut App) -> io::Result<()> {
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        let sync = scopeguard::guard((), |()| {
            let _ = execute!(io::stdout(), EndSynchronizedUpdate);
        });
        let res = (|| -> io::Result<()> {
            // The output mode changed: print the transcript again.
            let reprint = app.take_reprint();
            let split = app.is_diff_open() && self.terminal.size()?.width >= MIN_SPLIT_WIDTH;
            if split && self.split.is_none() {
                self.enter_split()?;
            } else if !split && self.split.is_some() {
                self.leave_split()?;
            }
            if let Some(split) = &mut self.split {
                if reprint {
                    // Printed again once the panel closes.
                    split.unprinted.clear();
                    split.cleared = true;
                }
                return self.draw_split(app);
            }
            let size = self.terminal.size()?;
            let desired = app.viewport_height(size.width).min(size.height.max(1));
            if self.pending_clear {
                self.transcript.clear();
            }
            if self.pending_clear || reprint || size.width != self.size.width {
                // Purge after clearing the screen: some terminals (kitty,
                // tmux) move a cleared screen into the scrollback.
                execute!(
                    io::stdout(),
                    Clear(ClearType::All),
                    Clear(ClearType::Purge),
                    MoveTo(0, 0),
                )?;
                let lines = app.render_tail(self.transcript.iter(), MAX_REPRINT_LINES);
                print_lines(&lines, size.width)?;
                self.recreate_viewport(desired)?;
            } else if size != self.size {
                let top = self.follow_cursor(size)?;
                execute!(
                    io::stdout(),
                    MoveTo(0, top),
                    Clear(ClearType::FromCursorDown)
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
            self.size = size;
            self.pending_clear = false;
            self.set_title(app)?;
            let entries = app.take_entries();
            flush_scrollback(&mut self.terminal, &app.render_entries(&entries))?;
            self.push_transcript(app, entries);
            self.terminal.draw(|frame| app.render(frame))?;
            ring_bell(app)?;
            self.cursor_row = match app.cursor_position() {
                Some(position) => position.y,
                None => {
                    // Park the hidden cursor where we can find it again.
                    let top = self.terminal.get_frame().area().y;
                    execute!(io::stdout(), MoveTo(0, top))?;
                    top
                }
            };
            Ok(())
        })();
        ScopeGuard::into_inner(sync);
        let end_res = execute!(io::stdout(), EndSynchronizedUpdate);
        res.and(end_res)
    }

    fn set_title(&mut self, app: &App) -> io::Result<()> {
        let title = app.title();
        if title != self.title {
            execute!(io::stdout(), SetTitle(&title))?;
            self.title = title;
        }
        Ok(())
    }

    /// Keep `entries` to print them again.
    fn push_transcript(&mut self, app: &App, entries: Vec<Entry>) {
        self.transcript.push(entries, |entry| app.freeze(entry));
    }

    fn enter_split(&mut self) -> io::Result<()> {
        let top = self.terminal.get_frame().area().y;
        execute!(io::stdout(), EnterAlternateScreen)?;
        ALTERNATE.store(true, Ordering::Relaxed);
        self.split = Some(Split {
            unprinted: Vec::new(),
            top,
            cleared: false,
        });
        let backend = CrosstermBackend::new(io::stdout());
        let options = TerminalOptions {
            viewport: Viewport::Fullscreen,
        };
        self.terminal = Terminal::with_options(backend, options)?;
        Ok(())
    }

    /// Go back to the inline viewport where it was, printing the lines
    /// added meanwhile above it; after a resize or a clear, print the
    /// whole transcript again instead.
    fn leave_split(&mut self) -> io::Result<()> {
        let Some(split) = self.split.take() else {
            return Ok(());
        };
        execute!(io::stdout(), LeaveAlternateScreen)?;
        ALTERNATE.store(false, Ordering::Relaxed);
        execute!(
            io::stdout(),
            MoveTo(0, split.top),
            Clear(ClearType::FromCursorDown)
        )?;
        self.recreate_viewport(self.height)?;
        self.cursor_row = split.top;
        if split.cleared || self.terminal.size()? != self.size {
            // The next draw prints the transcript again.
            self.size = Size::default();
            return Ok(());
        }
        flush_scrollback(&mut self.terminal, &split.unprinted)
    }

    fn draw_split(&mut self, app: &mut App) -> io::Result<()> {
        let entries = app.take_entries();
        if let Some(split) = &mut self.split {
            if self.pending_clear {
                self.pending_clear = false;
                self.transcript.clear();
                split.unprinted.clear();
                split.cleared = true;
            }
            if !split.cleared {
                split.unprinted.extend(app.render_entries(&entries));
            }
        }
        self.set_title(app)?;
        self.push_transcript(app, entries);
        let transcript = &self.transcript;
        self.terminal.draw(|frame| {
            let [left, right] =
                Layout::horizontal([Constraint::Fill(1), Constraint::Percentage(50)])
                    .areas(frame.area());
            // The viewport right below the transcript, as on the inline
            // screen.
            let height = app.viewport_height(left.width).min(left.height);
            let lines = app.render_tail(transcript.iter(), usize::from(left.height));
            let tail = last_rows(&lines, left.width, left.height - height);
            let rows = tail.len() as u16;
            let [history, viewport, _] = Layout::vertical([
                Constraint::Length(rows),
                Constraint::Length(height),
                Constraint::Fill(1),
            ])
            .areas(left);
            frame.render_widget(Paragraph::new(tail), history);
            app.render_in(frame, viewport);
            app.render_diff_panel(frame, right);
        })?;
        ring_bell(app)
    }

    /// Top row of the viewport after the terminal became `size`, found from
    /// how far the terminal moved the cursor.
    fn follow_cursor(&mut self, size: Size) -> io::Result<u16> {
        let top = self.terminal.get_frame().area().y;
        let row = self.terminal.get_cursor_position()?.y;
        let top = (top as i32 + row as i32 - self.cursor_row as i32)
            .clamp(0, size.height.saturating_sub(1) as i32);
        Ok(top as u16)
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
        // Print what the panel kept from the screen.
        let _ = self.leave_split();
        restore_terminal();
    }
}

/// Write `lines` into the terminal's native scrollback above the inline
/// viewport, wrapping to the current terminal width.
pub fn flush_scrollback<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: &[Line<'static>],
) -> Result<(), B::Error> {
    if lines.is_empty() {
        return Ok(());
    }
    let width = terminal.get_frame().area().width as usize;
    if width == 0 {
        return Ok(());
    }
    let wrapped = wrap_lines(lines, width);
    let height = wrapped.len().min(u16::MAX as usize) as u16;
    if height == 0 {
        return Ok(());
    }
    let paragraph = Paragraph::new(wrapped);
    terminal.insert_before(height, |buf| {
        paragraph.render(buf.area, buf);
    })
}

fn ring_bell(app: &mut App) -> io::Result<()> {
    if app.take_bell() {
        execute!(io::stdout(), Print('\x07'))?;
    }
    Ok(())
}

/// The last `height` rows of `lines` wrapped to `width`, wrapping only
/// the lines that show.
fn last_rows(lines: &[Line<'static>], width: u16, height: u16) -> Vec<Line<'static>> {
    let height = usize::from(height);
    let mut rows = Vec::with_capacity(height);
    for line in lines.iter().rev() {
        if rows.len() >= height {
            break;
        }
        let wrapped = wrap_lines(std::slice::from_ref(line), usize::from(width.max(1)));
        rows.extend(wrapped.into_iter().rev());
    }
    rows.truncate(height);
    rows.reverse();
    rows
}

/// Print `lines` at the cursor, wrapped to `width`, as plain terminal
/// output, without control characters: much less to write than drawing
/// them cell by cell.
fn print_lines(lines: &[Line<'static>], width: u16) -> io::Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    for line in lines {
        for row in wrap_lines(std::slice::from_ref(line), usize::from(width.max(1))) {
            for span in &row.spans {
                let style = row.style.patch(span.style).into_crossterm();
                // As a drawn cell holds none: text from the model or from
                // commands could otherwise send escape sequences.
                let text = span.content.replace(char::is_control, "");
                queue!(out, PrintStyledContent(StyledContent::new(style, text)))?;
            }
            queue!(out, Print("\r\n"))?;
        }
    }
    out.flush()
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Not for a thread the interface goes on without: it would be
            // left running on a terminal put back.
            let owner = *OWNER.lock().unwrap_or_else(PoisonError::into_inner);
            if owner.is_none_or(|owner| owner == thread::current().id()) {
                restore_terminal();
            }
            prev(info);
        }));
    });
}

/// Put the terminal back as [`InlineTerminal::enter`] found it; nothing
/// happens when it was not entered, or it was restored already.
pub(crate) fn restore_terminal() {
    if !ENTERED.swap(false, Ordering::Relaxed) {
        return;
    }
    let mut stdout = io::stdout();
    if ALTERNATE.swap(false, Ordering::Relaxed) {
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
    let _ = execute!(
        stdout,
        EndSynchronizedUpdate,
        DisableBracketedPaste,
        Print(POP_TITLE),
        Show
    );
    let _ = disable_raw_mode();
    let _ = writeln!(stdout);
    let _ = stdout.flush();
}
