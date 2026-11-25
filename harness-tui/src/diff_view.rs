// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The panel `/diff` opens: the files the session changed, and the diff of
//! the one selected.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};

use harness_core::tools::Diff;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::render_diff;
use crate::changes::{ChangeKind, ChangesUpdate, FileChange, Undo, UndoPlan, Watcher, MAX_FILES};
use crate::style::Theme;
use crate::text::char_width;

/// Spinner ticks between two scans while a tool runs: about 2 seconds.
const RESCAN_TICKS: u32 = 16;

/// How the panel tells how to use it.
const HINT: &str = "Alt+↑↓ file · PgUp/PgDn scroll";

#[derive(Default)]
pub(crate) struct DiffView {
    /// The project folder, watched from the start of the session.
    root: Option<PathBuf>,
    watcher: Option<Watcher>,
    open: bool,
    /// The changed files, unknown until the first scan.
    files: Option<Vec<FileChange>>,
    truncated: bool,
    selected: usize,
    /// The diff last received, and the file it is of.
    diff: Option<(PathBuf, Result<Diff, &'static str>)>,
    /// First diff row shown.
    scroll: usize,
    ticks: u32,
    /// Diff rows shown, and how many there are, at the last render.
    page: Cell<usize>,
    rows: Cell<usize>,
}

impl DiffView {
    /// Take a snapshot of `root` now, to compare its files with later.
    pub fn watch(&mut self, root: PathBuf) -> io::Result<()> {
        self.watcher = Some(Watcher::spawn(root.clone())?);
        self.root = Some(root);
        self.files = None;
        self.diff = None;
        self.selected = 0;
        self.scroll = 0;
        self.refresh();
        Ok(())
    }

    /// Take a new snapshot, for a new session.
    pub fn restart(&mut self) -> io::Result<()> {
        // Kept when the watcher does not start, for the next session.
        match self.root.clone() {
            Some(root) => self.watch(root),
            None => Ok(()),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.refresh();
    }

    /// Look for changes again, when the panel shows them.
    pub fn refresh(&self) {
        if let Some(watcher) = self.watcher.as_ref().filter(|_| self.open) {
            watcher.scan();
        }
    }

    /// Look for changes now and then while a tool runs.
    pub fn tick(&mut self, running: bool) {
        if self.open && running {
            self.ticks = (self.ticks + 1) % RESCAN_TICKS;
            if self.ticks == 0 {
                self.refresh();
            }
        }
    }

    /// Remember the files as they are, before a prompt joins a
    /// conversation of `messages` messages, the last one told by `tag`.
    pub async fn checkpoint(&self, messages: usize, tag: u64) {
        if let Some(watcher) = &self.watcher {
            watcher.checkpoint(messages, tag).await;
        }
    }

    /// What [`Self::undo`] would do, `None` when there is nothing to undo.
    pub async fn undo_plan(&self) -> Option<UndoPlan> {
        self.watcher.as_ref()?.undo_plan().await
    }

    /// Put the files back as they were before the last prompt, as the
    /// plan `shown` says: see [`Watcher::undo`].
    pub async fn undo(&self, shown: UndoPlan) -> Option<Result<Undo, UndoPlan>> {
        let undo = self.watcher.as_ref()?.undo(shown).await;
        self.refresh();
        undo
    }

    /// The messages and the tag the last checkpoint was taken with.
    pub async fn last_checkpoint(&self) -> Option<(usize, u64)> {
        self.watcher.as_ref()?.last_checkpoint().await
    }

    /// Forget the files as they were before the last prompt, or before
    /// `all` of them.
    pub fn forget_checkpoints(&self, all: bool) {
        if let Some(watcher) = &self.watcher {
            watcher.forget_checkpoints(all);
        }
    }

    /// What the watcher found next; never, without one.
    pub async fn next(&mut self) -> ChangesUpdate {
        if let Some(watcher) = &mut self.watcher {
            if let Some(update) = watcher.next().await {
                return update;
            }
        }
        std::future::pending().await
    }

    pub fn apply(&mut self, update: ChangesUpdate) {
        match update {
            ChangesUpdate::Files { files, truncated } => {
                let selected = self.selected_path().map(Path::to_owned);
                self.selected = selected
                    .and_then(|path| files.iter().position(|f| f.path == path))
                    .unwrap_or_else(|| self.selected.min(files.len().saturating_sub(1)));
                self.files = Some(files);
                self.truncated = truncated;
                self.request_diff();
            }
            ChangesUpdate::Diff { path, diff } => {
                if self.selected_path() != Some(path.as_path()) {
                    return;
                }
                if self.diff.as_ref().is_some_and(|(old, _)| *old != path) {
                    self.scroll = 0;
                }
                self.diff = Some((path, diff));
            }
        }
    }

    /// Select the file `delta` rows away; returns whether it moved.
    pub fn select(&mut self, delta: isize) -> bool {
        let count = self.files.as_ref().map_or(0, Vec::len);
        let selected = self
            .selected
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
        if selected == self.selected {
            return false;
        }
        self.selected = selected;
        self.request_diff();
        true
    }

    /// Scroll the diff a page down or up; returns whether it moved.
    pub fn scroll_page(&mut self, down: bool) -> bool {
        let page = self.page.get().max(1);
        let last = self.rows.get().saturating_sub(page);
        let scroll = if down {
            (self.scroll + page).min(last)
        } else {
            self.scroll.saturating_sub(page)
        };
        let moved = scroll != self.scroll;
        self.scroll = scroll;
        moved
    }

    fn selected_path(&self) -> Option<&Path> {
        let files = self.files.as_ref()?;
        files.get(self.selected).map(|f| f.path.as_path())
    }

    fn request_diff(&self) {
        if let (Some(watcher), Some(path)) = (&self.watcher, self.selected_path()) {
            watcher.diff(path.to_owned());
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: Theme) {
        let block = Block::new()
            .borders(Borders::LEFT)
            .border_style(theme.border_idle);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(files) = &self.files else {
            let line = Line::styled(" Looking at the files…", theme.muted);
            frame.render_widget(Paragraph::new(line), inner);
            return;
        };
        let list_h = (files.len().max(1) as u16).min((inner.height / 3).max(3));
        let [header, list, title, body, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(list_h),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        frame.render_widget(Paragraph::new(self.header(files, theme)), header);
        self.render_list(frame, list, files, theme);
        if let Some(path) = self.selected_path() {
            let name = Line::styled(
                format!(
                    " {}",
                    fit_path(path, title.width.saturating_sub(1) as usize)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            );
            frame.render_widget(Paragraph::new(name), title);
            self.render_diff(frame, body, path, theme);
        }
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {HINT}"), theme.muted)),
            hint,
        );
    }

    fn header(&self, files: &[FileChange], theme: Theme) -> Line<'static> {
        let (added, removed) = files
            .iter()
            .filter_map(|f| f.lines)
            .fold((0, 0), |(a, r), (fa, fr)| (a + fa, r + fr));
        let count = match files.len() {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        let mut spans = vec![Span::styled(" Changes", theme.prompt)];
        if !files.is_empty() {
            spans.extend([
                Span::styled(format!(" {count} "), theme.muted),
                Span::styled(format!("+{added} "), theme.diff_add),
                Span::styled(format!("-{removed}"), theme.diff_remove),
            ]);
        }
        if self.truncated {
            spans.push(Span::styled(
                format!(" (first {MAX_FILES} files)"),
                theme.muted,
            ));
        }
        Line::from(spans)
    }

    fn render_list(&self, frame: &mut Frame<'_>, area: Rect, files: &[FileChange], theme: Theme) {
        if files.is_empty() {
            let line = Line::styled(" No changes since the session started", theme.muted);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
        let visible = area.height as usize;
        let start = self.selected.saturating_sub(visible.saturating_sub(1));
        let width = area.width as usize;
        let lines: Vec<Line<'static>> = files
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, file)| file_line(file, i == self.selected, width, theme))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_diff(&self, frame: &mut Frame<'_>, area: Rect, path: &Path, theme: Theme) {
        let mut lines = Vec::new();
        match &self.diff {
            Some((of, Ok(diff))) if of == path && diff.hunks.is_empty() => {
                lines.push(Line::styled(" no changes", theme.muted));
            }
            Some((of, Ok(diff))) if of == path => render_diff(diff, usize::MAX, theme, &mut lines),
            Some((of, Err(reason))) if of == path => {
                lines.push(Line::styled(format!(" {reason}"), theme.muted));
            }
            _ => lines.push(Line::styled(" …", theme.muted)),
        }
        self.page.set(area.height as usize);
        self.rows.set(lines.len());
        let scroll = self.scroll.min(lines.len().saturating_sub(1));
        lines.drain(..scroll);
        lines.truncate(area.height as usize);
        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// A row of the list: the kind of change, the path, and the lines added
/// and removed on the right.
fn file_line(file: &FileChange, selected: bool, width: usize, theme: Theme) -> Line<'static> {
    let (mark, mark_style) = match file.kind {
        ChangeKind::Added => ('A', theme.diff_add),
        ChangeKind::Modified => ('M', theme.prompt),
        ChangeKind::Deleted => ('D', theme.diff_remove),
    };
    let counts = match file.lines {
        Some((added, removed)) => vec![
            Span::styled(format!("+{added}"), theme.diff_add),
            Span::raw(" "),
            Span::styled(format!("-{removed}"), theme.diff_remove),
        ],
        None => vec![Span::styled("~", theme.muted)],
    };
    let counts_w: usize = counts.iter().map(|s| s.content.width()).sum();
    let path_w = width.saturating_sub(counts_w + 5);
    let path = fit_path(&file.path, path_w);
    let gap = path_w.saturating_sub(path.width()) + 1;
    let path_style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(mark.to_string(), mark_style),
        Span::raw(" "),
        Span::styled(path, path_style),
        Span::raw(" ".repeat(gap)),
    ];
    spans.extend(counts);
    Line::from(spans)
}

/// `path` in `width` cells at most, cut at the start, where it matters
/// least.
fn fit_path(path: &Path, width: usize) -> String {
    let text = path.to_string_lossy();
    if text.width() <= width {
        return text.into_owned();
    }
    let mut kept = 0;
    let mut start = text.len();
    for (i, ch) in text.char_indices().rev() {
        let w = char_width(ch);
        if kept + w + 1 > width {
            break;
        }
        kept += w;
        start = i;
    }
    format!("…{}", &text[start..])
}
