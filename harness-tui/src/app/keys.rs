// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What the keys do.

use super::*;

impl App {
    /// Process a crossterm terminal event and return the next action.
    pub fn handle_event(&mut self, event: TermEvent) -> Action {
        match event {
            TermEvent::Resize(w, _) => {
                if w > 0 {
                    self.set_width(w);
                    if self.show_thinking && !self.live_reasoning.is_empty() {
                        stream_reasoning(
                            &mut self.live_reasoning,
                            "",
                            w as usize,
                            self.theme.reasoning,
                            &mut self.scrollback,
                        );
                    }
                }
                Action::Redraw
            }
            TermEvent::Paste(text) => {
                if let Some(picker) = &mut self.picker {
                    return if picker.paste(&text) {
                        Action::Redraw
                    } else {
                        Action::None
                    };
                }
                self.notice = None;
                if self.input.paste(&text) {
                    self.completion.update(self.input.text());
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            _ => Action::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if let Some(picker) = &mut self.picker {
            return match picker.handle_key(key) {
                PickerOutcome::None => Action::None,
                PickerOutcome::Redraw => Action::Redraw,
                PickerOutcome::Select(kind, id) => {
                    self.picker = None;
                    match kind {
                        PickerKind::Session => Action::ResumeSession(id),
                        PickerKind::Model => Action::SelectModel(id),
                        PickerKind::Answer => Action::Answer(id),
                    }
                }
                PickerOutcome::Cancel => match self.picker.take().map(|p| p.kind()) {
                    // The model waits for the answer: cancel the prompt.
                    Some(PickerKind::Answer) => Action::Cancel,
                    _ => Action::Redraw,
                },
                PickerOutcome::Quit => {
                    self.picker = None;
                    Action::Quit
                }
            };
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('o'), true) => {
                self.toggle_thinking();
                return Action::Redraw;
            }
            (KeyCode::Char('c'), true) => {
                if self.busy {
                    return Action::Cancel;
                }
                if !self.input.is_empty() {
                    self.input.clear();
                    self.completion.clear();
                    self.notice = None;
                } else {
                    self.notice = Some("Press Ctrl-D to quit".to_owned());
                }
                return Action::Redraw;
            }
            (KeyCode::Esc, false) if self.busy => return Action::Cancel,
            (KeyCode::Char('d'), true) => {
                if self.busy || self.input.is_empty() {
                    return Action::Quit;
                }
                return Action::None;
            }
            _ => {}
        }

        if self.diff.is_open() {
            let select = key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
            let moved = match key.code {
                KeyCode::Up if select => Some(self.diff.select(-1)),
                KeyCode::Down if select => Some(self.diff.select(1)),
                KeyCode::PageUp => Some(self.diff.scroll_page(false)),
                KeyCode::PageDown => Some(self.diff.scroll_page(true)),
                _ => None,
            };
            if let Some(moved) = moved {
                return if moved { Action::Redraw } else { Action::None };
            }
        }

        if key.code == KeyCode::BackTab && !self.busy {
            self.plan = !self.plan;
            self.notice = Some(
                if self.plan {
                    "Plan mode: the model plans and changes nothing (Shift+Tab to leave)"
                } else {
                    "Regular mode: the model may change files and run commands"
                }
                .to_owned(),
            );
            return Action::SetPlan(self.plan);
        }

        if self.completion.is_open() {
            match (key.code, ctrl) {
                (KeyCode::Esc, false) => {
                    self.completion.dismiss();
                    return Action::Redraw;
                }
                (KeyCode::Up, false) | (KeyCode::Char('p'), true) => {
                    return if self.completion.move_up() {
                        Action::Redraw
                    } else {
                        Action::None
                    };
                }
                (KeyCode::Down, false) | (KeyCode::Char('n'), true) => {
                    return if self.completion.move_down() {
                        Action::Redraw
                    } else {
                        Action::None
                    };
                }
                (KeyCode::Tab, false) => {
                    if let Some(insert) = self.completion.selected_item().map(|i| i.insert.clone())
                    {
                        self.input.set_text(&insert);
                        self.completion.update(self.input.text());
                        return Action::Redraw;
                    }
                }
                _ => {}
            }
        }

        self.notice = None;
        if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            if let Some(insert) = self.completion.selected_item().map(|i| i.insert.clone()) {
                let trimmed = self.input.text().trim();
                let already_complete = trimmed.eq_ignore_ascii_case(insert.trim())
                    || matches!(parse_slash(trimmed), Some(Ok(SlashCommand::Thinking(None))));
                if !already_complete || self.completion.navigated() {
                    if insert.ends_with(' ') {
                        self.input.set_text(&insert);
                        self.completion.update(self.input.text());
                        return Action::Redraw;
                    }
                    self.input.set_text(&insert);
                }
            }
            // The only command that does not wait for the prompt to end.
            if matches!(
                parse_slash(self.input.text().trim()),
                Some(Ok(SlashCommand::Diff))
            ) {
                self.input.submit();
                self.completion.clear();
                self.toggle_diff();
                return Action::Redraw;
            }
            // Commands wait for the prompt to end.
            if self.busy && self.input.text().trim_start().starts_with(['/', '!']) {
                return Action::None;
            }
            self.completion.clear();
            let text = self.input.submit();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Action::Redraw;
            }
            match parse_slash(trimmed) {
                Some(Ok(cmd)) => return Action::Slash(cmd),
                // A custom command, sent once expanded, or a skill, which
                // the model reads as it is.
                Some(Err(unknown)) if self.completion.is_custom(&unknown[1..]) => {}
                Some(Err(unknown)) => {
                    self.push_info(format!("unknown command: {unknown} (try /help)"));
                    return Action::Redraw;
                }
                None => {}
            }
            if let Some(command) = trimmed.strip_prefix('!') {
                return Action::Shell(command.trim_start().to_owned());
            }
            if self.busy {
                self.queued += 1;
            }
            return Action::Submit(trimmed.to_owned());
        }

        if self.input.handle_key(key) {
            self.completion.update(self.input.text());
            Action::Redraw
        } else {
            Action::None
        }
    }
}
