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
                        let mut lines = Vec::new();
                        stream_reasoning(
                            &mut self.live_reasoning,
                            "",
                            w as usize,
                            self.theme.reasoning,
                            &mut lines,
                        );
                        self.push_lines(lines);
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
        if self.picker.is_some() {
            return self.picker_key(key);
        }
        if let Some(action) = self.control_key(key) {
            return action;
        }
        if let Some(action) = self.diff_key(key) {
            return action;
        }
        if key.code == KeyCode::BackTab && !self.busy {
            return self.toggle_plan();
        }
        if key.code == KeyCode::Char('v') && key.modifiers == KeyModifiers::CONTROL {
            if self.busy {
                self.notice = Some("Images can be pasted once the prompt ends".to_owned());
                return Action::Redraw;
            }
            return Action::PasteImage;
        }
        if let Some(action) = self.completion_key(key) {
            return action;
        }
        self.notice = None;
        if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            return self.submit();
        }
        if self.input.handle_key(key) {
            self.completion.update(self.input.text());
            Action::Redraw
        } else {
            Action::None
        }
    }

    /// A key while a picker is open: it takes them all.
    fn picker_key(&mut self, key: KeyEvent) -> Action {
        let Some(picker) = &mut self.picker else {
            return Action::None;
        };
        match picker.handle_key(key) {
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
        }
    }

    /// The keys that work whatever is open: show the thinking, cancel,
    /// quit.
    fn control_key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('o'), true) => {
                self.toggle_thinking();
                Some(Action::Redraw)
            }
            (KeyCode::Char('c'), true) => {
                if self.busy {
                    return Some(Action::Cancel);
                }
                if !self.input.is_empty() {
                    self.input.clear();
                    self.images.clear();
                    self.completion.clear();
                    self.notice = None;
                } else {
                    self.notice = Some("Press Ctrl-D to quit".to_owned());
                }
                Some(Action::Redraw)
            }
            (KeyCode::Esc, false) if self.busy => Some(Action::Cancel),
            (KeyCode::Char('d'), true) => {
                if self.busy || self.input.is_empty() {
                    return Some(Action::Quit);
                }
                Some(Action::None)
            }
            _ => None,
        }
    }

    /// The keys that move in the `/diff` view, while it is open.
    fn diff_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.diff.is_open() {
            return None;
        }
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
        moved.map(|moved| if moved { Action::Redraw } else { Action::None })
    }

    /// Switch between plan mode and regular mode.
    fn toggle_plan(&mut self) -> Action {
        self.plan = !self.plan;
        self.notice = Some(
            if self.plan {
                "Plan mode: the model plans and changes nothing (Shift+Tab to leave)"
            } else {
                "Regular mode: the model may change files and run commands"
            }
            .to_owned(),
        );
        Action::SetPlan(self.plan)
    }

    /// The keys that pick a completion, while the list is open.
    fn completion_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.completion.is_open() {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let moved = |moved: bool| if moved { Action::Redraw } else { Action::None };
        match (key.code, ctrl) {
            (KeyCode::Esc, false) => {
                self.completion.dismiss();
                Some(Action::Redraw)
            }
            (KeyCode::Up, false) | (KeyCode::Char('p'), true) => {
                Some(moved(self.completion.move_up()))
            }
            (KeyCode::Down, false) | (KeyCode::Char('n'), true) => {
                Some(moved(self.completion.move_down()))
            }
            (KeyCode::Tab, false) => {
                let insert = self.completion.selected_item()?.insert.clone();
                self.input.set_text(&insert);
                self.completion.update(self.input.text());
                Some(Action::Redraw)
            }
            _ => None,
        }
    }

    /// Enter: complete the command being typed, or send what was typed.
    fn submit(&mut self) -> Action {
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
            self.queued.push(trimmed.to_owned());
        }
        Action::Submit(trimmed.to_owned())
    }
}
