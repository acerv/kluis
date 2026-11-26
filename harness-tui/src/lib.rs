// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Inline terminal user interface for LLM harnesses, keeping the native
//! terminal scrollback.
//!
//! Only Linux is supported.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-tui only supports Linux");

mod app;
mod changes;
mod clipboard;
mod complete;
mod diff_view;
mod drive;
mod history;
mod input;
pub mod json_lines;
mod markdown;
mod picker;
mod session;
mod signals;
mod slash;
mod stats;
mod style;
mod terminal;
mod text;
mod transcript;

use std::io::Write;
use std::path::PathBuf;

use genai::adapter::AdapterKind;
use genai::chat::ChatMessage;
use harness_core::tools::is_read_only;
use harness_core::{expand_command, Agent, Command, Error, SessionSummary};
use harness_sandbox::GitGuard;
use tokio::sync::mpsc;

pub use app::{
    printable, pruned_notice, retry_notice, summary, Action, AgentUpdate, App, BannerLine,
    SlashCommand, TRUNCATED_NOTICE, VIEWPORT_HEIGHT,
};
pub use changes::{
    ChangeKind, ChangesUpdate, FileChange, Snapshot, Undo, UndoPlan, Watcher, MAX_CHECKPOINTS,
    MAX_FILES, MAX_SNAPSHOT_BYTES, MAX_TEXT_BYTES,
};
pub use clipboard::{last_assistant_text, osc52_sequence, paste_image};
pub use complete::is_builtin;
pub use history::{History, MAX_HISTORY};
pub use markdown::code_theme;
pub use session::{PromptSaver, Session};
pub use signals::Signals;
pub use stats::PromptStats;
pub use style::{Theme, ThemeName};
pub use terminal::flush_scrollback;
pub use tui_markdown::CodeTheme;

use drive::{handle_event_batch, restore_queued, run_shell, run_turn, Ui};
use slash::{
    check_thinking, handle_slash, history_tag, resume_session, switch_model, sync_checkpoints,
    undo, ModelCatalog,
};
use terminal::{EventReader, InlineTerminal};

/// `print!` that goes on when nobody reads the standard output any more,
/// as in `kl -p ... | head`: the panic of `print!` would skip what ends a
/// prompt, the check for new repositories included.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = write!(std::io::stdout(), $($arg)*);
    }};
}

/// `println!` that goes on like [`out!`].
#[macro_export]
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// Session details displayed in the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Sandbox mode label (e.g. `"sandboxed"`, `"strict sandbox"`, `"no sandbox"`).
    pub sandbox: String,
    /// Working folder label shown in the status bar.
    pub folder: String,
}

/// Optional state passed into [`run`].
#[derive(Default)]
pub struct RunOptions {
    pub session: Option<Session>,
    /// The model the user asked for, which the session picked at startup
    /// switches to: it keeps its own model otherwise.
    pub model: Option<String>,
    pub history: Option<History>,
    pub picker_entries: Vec<SessionSummary>,
    /// Go on in a copy of the session picked at startup.
    pub fork: bool,
    pub adapters: Vec<AdapterKind>,
    /// Shown next to the startup logo.
    pub banner: Vec<BannerLine>,
    /// Checked after every prompt, when sandboxed.
    pub git_guard: Option<GitGuard>,
    /// Colors for a dark or light terminal.
    pub theme: ThemeName,
    /// Colors of code blocks, instead of the default of `theme`.
    pub code_theme: Option<CodeTheme>,
    /// The custom commands `/<name>` runs.
    pub commands: Vec<Command>,
    /// Ring the bell when a long prompt ends or asks a question.
    pub notify: bool,
    /// The user's own `AGENTS.md`, which `/remember --global` adds to.
    pub agents_file: Option<PathBuf>,
}

/// Disable the git repositories commands created during a prompt that
/// added `messages` to the history, returning what to tell the user.
///
/// Only a tool that may change files can create one, so without such a
/// call the folders are not scanned.
pub fn git_notices(guard: Option<&mut GitGuard>, messages: &[ChatMessage]) -> Vec<String> {
    let changes_files = messages
        .iter()
        .flat_map(|message| message.content.tool_calls())
        .any(|call| !is_read_only(&call.fn_name));
    check_git_guard(guard, changes_files)
}

/// Like [`git_notices`], when the caller already tracked whether any tool call
/// in the prompt could change files (even if auto-compaction replaced history).
fn check_git_guard(guard: Option<&mut GitGuard>, changed_files: bool) -> Vec<String> {
    let Some(guard) = guard.filter(|_| changed_files) else {
        return Vec::new();
    };
    let check = guard.disable_new();
    let new = check.new.into_iter().map(|repo| match repo.moved_to {
        Ok(to) => format!(
            "[a command created the git repository {}: renamed to {}, check its \
             config and hooks before renaming it back]",
            repo.path.display(),
            to.display()
        ),
        Err(err) => format!(
            "[warning: a command created the git repository {} and it could not \
             be disabled ({err}): check its config and hooks before using git there]",
            repo.path.display()
        ),
    });
    let unchecked = check
        .unchecked
        .into_iter()
        .map(|err| format!("[warning: {err}; check it before using git there]"));
    new.chain(unchecked).collect()
}

/// Run the interactive inline TUI until the user quits (`Ctrl-D` or `/quit`)
/// or one of `signals` asks to.
pub async fn run(
    agent: Agent,
    status: Status,
    mut signals: Signals,
    opts: RunOptions,
) -> Result<(), Error> {
    let (asker, questions) = mpsc::unbounded_channel();
    let mut agent = agent.with_asker(asker);
    let RunOptions {
        mut session,
        model: mut startup_model,
        mut history,
        picker_entries,
        fork: mut startup_fork,
        adapters,
        banner,
        mut git_guard,
        theme,
        code_theme,
        commands,
        notify,
        agents_file,
    } = opts;
    let mut models = ModelCatalog::new(adapters);
    let term = InlineTerminal::enter(VIEWPORT_HEIGHT)?;
    let mut app = App::new(agent.model().to_owned(), status, Theme::from_env(theme));
    app.set_width(term.width());
    app.set_thinking(agent.thinking());
    app.set_notify(notify);
    app.set_compact_at(agent.auto_compact());
    let code_theme = match code_theme {
        Some(code_theme) => code_theme,
        None => markdown::default_code_theme(theme == ThemeName::Light)?,
    };
    app.set_code_theme(code_theme);
    app.set_custom(&commands, agent.skills());
    if let Ok(cwd) = std::env::current_dir() {
        if let Err(err) = app.watch_changes(cwd.clone()) {
            app.push_info(format!("[/diff unavailable: {err}]"));
        }
        app.set_workspace(cwd);
    }
    if let Some(ref h) = history {
        app.set_history(h.entries());
    }
    app.push_banner(&banner);
    if !agent.history().is_empty() {
        app.replay_history(agent.history());
        if let Some(session) = session.as_ref() {
            app.set_usage(session.usage());
            app.set_context(session.context());
        }
    } else if !picker_entries.is_empty() {
        app.open_picker(picker_entries, None);
    }
    let events = EventReader::spawn();

    let mut ui = Ui {
        term,
        app,
        events,
        questions,
        signals: signals.clone(),
        agents_file,
        undo_asked: None,
    };
    ui.draw()?;

    // A prompt to send next, without waiting for the user.
    let mut next_prompt = None;
    // The model whose thinking levels were last checked.
    let mut checked_model = String::new();
    loop {
        if checked_model != agent.model() {
            checked_model = agent.model().to_owned();
            tokio::select! {
                _ = signals.recv() => break,
                () = check_thinking(&mut agent, &mut ui.app) => {}
            }
            ui.draw()?;
        }
        let action = match next_prompt.take() {
            Some(prompt) => Action::Submit(prompt),
            None => {
                let next = tokio::select! {
                    _ = signals.recv() => break,
                    next = ui.events.next() => next,
                    update = ui.app.next_changes() => {
                        ui.app.apply_changes(update);
                        ui.draw()?;
                        continue;
                    }
                };
                let Some(next) = next else {
                    break;
                };
                handle_event_batch(next?, &mut ui.events, &mut ui.app)?
            }
        };
        // Only for the picker shown at startup, until anything else is done.
        let model = match action {
            Action::None | Action::Redraw => None,
            _ => startup_model.take(),
        };
        let fork = match action {
            Action::None | Action::Redraw => false,
            _ => std::mem::take(&mut startup_fork),
        };
        match action {
            Action::Answer(answer) if ui.undo_asked.is_some() => {
                if let Some(shown) = ui.undo_asked.take() {
                    ui.undo_asked =
                        undo(&mut ui.app, &answer, shown, &mut agent, session.as_mut()).await;
                }
                ui.draw()?;
            }
            Action::Cancel if ui.undo_asked.is_some() => {
                ui.undo_asked = None;
                ui.draw()?;
            }
            Action::None | Action::Cancel | Action::Answer(_) => {}
            Action::Redraw => ui.draw()?,
            Action::Quit => break,
            Action::Slash(cmd) => {
                let slash = handle_slash(
                    &mut ui,
                    cmd,
                    &mut agent,
                    session.as_mut(),
                    &mut models,
                    &mut next_prompt,
                );
                let quit = tokio::select! {
                    _ = signals.recv() => true,
                    quit = slash => quit?,
                };
                if quit {
                    break;
                }
                ui.draw()?;
            }
            Action::ResumeSession(id) => {
                resume_session(&id, model, fork, &mut agent, &mut ui.app, session.as_mut());
                ui.draw()?;
            }
            Action::SelectModel(name) => {
                switch_model(&name, &mut agent, &mut ui.app, session.as_mut());
                ui.draw()?;
            }
            Action::SetPlan(plan) => {
                agent.set_plan(plan);
                ui.draw()?;
            }
            Action::PasteImage => {
                match clipboard::paste_image() {
                    Ok(image) => ui.app.paste_image(image),
                    Err(err) => ui.app.push_info(format!("[paste error: {err}]")),
                }
                ui.draw()?;
            }
            Action::Shell(command) => {
                if let Some(ref mut h) = history {
                    let _ = h.record(&format!("!{command}"));
                }
                let quit = run_shell(&mut ui, &agent, &command).await?;
                let notices = after_commands(&agent, git_guard.as_mut(), true, !quit).await;
                if quit {
                    quit_with(ui.term, &notices);
                    return Ok(());
                }
                show_notices(&mut ui, &notices)?;
            }
            Action::Submit(prompt) => {
                if let Some(ref mut h) = history {
                    let _ = h.record(&prompt);
                }
                let prompt = expand_command(&commands, &prompt).unwrap_or(prompt);
                for (name, image) in ui.app.take_images(&prompt) {
                    agent.attach_image(name, image);
                }
                // What `/undo` goes back to.
                let history = agent.history();
                ui.app.checkpoint(history.len(), history_tag(history)).await;
                ui.app.start_prompt(&prompt);
                ui.draw()?;
                let turn = run_turn(&mut ui, &mut agent, session.as_mut(), &prompt).await?;
                sync_checkpoints(&ui.app, &agent).await;
                let notices =
                    after_commands(&agent, git_guard.as_mut(), turn.changed_files, !turn.quit)
                        .await;
                if turn.quit {
                    quit_with(ui.term, &notices);
                    return Ok(());
                }
                show_notices(&mut ui, &notices)?;
                // Messages queued as the prompt ended make the next one; a
                // cancelled prompt leaves them in the editor.
                if turn.cancelled {
                    restore_queued(&agent, &mut ui.app);
                    ui.draw()?;
                } else {
                    let queued = agent.steering().take().join("\n\n");
                    next_prompt = (!queued.is_empty()).then_some(queued);
                }
            }
        }
    }
    let notices = after_commands(&agent, git_guard.as_mut(), false, false).await;
    quit_with(ui.term, &notices);
    Ok(())
}

/// Stop what commands left running in the background, but the commands
/// started in the background with `keep_jobs`, then disable the git
/// repositories they created when they may have changed files, returning
/// what to tell the user.
pub async fn after_commands(
    agent: &Agent,
    guard: Option<&mut GitGuard>,
    changed_files: bool,
    keep_jobs: bool,
) -> Vec<String> {
    // Before the git check: a program left running could recreate a
    // repository right after it. One kept running still can, until the
    // check after the next prompt, or the last one.
    let jobs = agent.stop_background(keep_jobs).await;
    check_git_guard(guard, changed_files || jobs)
}

/// Leave the TUI, printing `notices` once the screen is back.
fn quit_with(term: InlineTerminal, notices: &[String]) {
    drop(term);
    // Without `eprintln`: the terminal may be gone, after a `SIGHUP`.
    let mut stderr = std::io::stderr().lock();
    for notice in notices {
        let _ = writeln!(stderr, "{}", printable(notice));
    }
}

fn show_notices(ui: &mut Ui, notices: &[String]) -> Result<(), Error> {
    if notices.is_empty() {
        return Ok(());
    }
    notices.iter().for_each(|notice| ui.app.push_info(notice));
    ui.draw()
}
