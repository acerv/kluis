// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Inline terminal user interface for LLM harnesses, keeping the native
//! terminal scrollback.
//!
//! Only Linux is supported.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-tui only supports Linux");

mod app;
mod input;
mod markdown;
mod style;
mod terminal;

use std::time::Duration;

use harness_core::{Agent, Error};
use tokio::sync::mpsc;
use tokio::time::interval;

pub use app::{summary, Action, AgentUpdate, App, SlashCommand, VIEWPORT_HEIGHT};
pub use style::Theme;
pub use terminal::flush_scrollback;

use terminal::{EventReader, InlineTerminal};

const SPINNER_TICK: Duration = Duration::from_millis(120);

/// Session details displayed in the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Sandbox mode label (e.g. `"sandboxed"`, `"strict sandbox"`, `"no sandbox"`).
    pub sandbox: String,
    /// Working folder label shown in the status bar.
    pub folder: String,
    /// Current git branch when inside a repository.
    pub git_branch: Option<String>,
}

/// Run the interactive inline TUI until the user quits (`Ctrl-D` or `/quit`).
pub async fn run(mut agent: Agent, status: Status) -> Result<(), Error> {
    let mut term = InlineTerminal::enter(VIEWPORT_HEIGHT)?;
    let mut app = App::new(agent.model().to_owned(), status, Theme::from_env());
    let mut events = EventReader::spawn();

    draw(&mut term, &mut app)?;

    loop {
        let Some(next) = events.next().await else {
            break;
        };
        let ev = next?;
        match app.handle_event(ev) {
            Action::None | Action::Cancel => {}
            Action::Redraw => draw(&mut term, &mut app)?,
            Action::Quit => break,
            Action::Slash(cmd) => {
                if handle_slash(cmd, &mut agent, &mut app) {
                    break;
                }
                draw(&mut term, &mut app)?;
            }
            Action::Submit(prompt) => {
                app.start_prompt(&prompt);
                draw(&mut term, &mut app)?;
                if run_turn(&mut term, &mut app, &mut agent, &mut events, &prompt).await? {
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Execute a slash command. Returns `true` when the user requested to quit.
fn handle_slash(cmd: SlashCommand, agent: &mut Agent, app: &mut App) -> bool {
    match cmd {
        SlashCommand::Quit => return true,
        SlashCommand::Clear => {
            agent.clear();
            app.reset_tokens();
            app.push_info("[conversation cleared]");
        }
        SlashCommand::Model(Some(name)) => {
            agent.set_model(&name);
            app.set_model(&name);
            app.push_info(format!("model: {name}"));
        }
        SlashCommand::Model(None) => {
            app.push_info(format!("model: {}", agent.model()));
        }
        SlashCommand::Help => {
            app.push_info(
                "commands: /model [name], /clear, /quit · keys: Enter send, Shift/Alt+Enter newline, Up/Down history, Ctrl-T toggle thinking, Esc/Ctrl-C cancel, Ctrl-D quit",
            );
        }
    }
    false
}

/// Run one agent prompt while keeping the UI responsive to keys, resize and
/// streaming updates. Returns `true` if the user requested to quit.
async fn run_turn(
    term: &mut InlineTerminal,
    app: &mut App,
    agent: &mut Agent,
    events: &mut EventReader,
    prompt: &str,
) -> Result<bool, Error> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let mut cancelled = false;
    let mut quit = false;
    let mut outcome: Option<Result<String, Error>> = None;

    // Scope `prompt_fut` so dropping it on exit runs `Agent::prompt`'s cancel
    // cleanup and closes `tx` before we drain `rx`.
    {
        let prompt_fut = agent.prompt(prompt, |event| {
            if let Some(update) = AgentUpdate::from_event(event) {
                let _ = tx.send(update);
            }
        });
        tokio::pin!(prompt_fut);

        let mut ticker = interval(SPINNER_TICK);

        loop {
            tokio::select! {
                biased;
                maybe_ev = events.next() => {
                    match maybe_ev {
                        Some(Ok(ev)) => match app.handle_event(ev) {
                            Action::Cancel => {
                                cancelled = true;
                                break;
                            }
                            Action::Quit => {
                                quit = true;
                                break;
                            }
                            Action::Redraw => draw(term, app)?,
                            Action::None | Action::Submit(_) | Action::Slash(_) => {}
                        },
                        Some(Err(err)) => return Err(err.into()),
                        None => {
                            quit = true;
                            break;
                        }
                    }
                }
                Some(update) = rx.recv() => {
                    app.apply_update(update);
                    draw(term, app)?;
                }
                res = &mut prompt_fut => {
                    outcome = Some(res);
                    break;
                }
                _ = ticker.tick() => {
                    app.tick();
                    draw(term, app)?;
                }
            }
        }
    }

    while let Ok(update) = rx.try_recv() {
        app.apply_update(update);
    }

    if quit {
        return Ok(true);
    }
    if cancelled {
        app.cancel_prompt();
    } else if let Some(res) = outcome {
        app.finish_prompt(res.map(|_| ()).map_err(|e| e.to_string()));
    }
    draw(term, app)?;
    Ok(false)
}

fn draw(term: &mut InlineTerminal, app: &mut App) -> Result<(), Error> {
    term.draw_app(app)?;
    Ok(())
}
