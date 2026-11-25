// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Running the agent while the terminal stays live.

use std::path::PathBuf;
use std::time::Duration;

use harness_core::tools::{Question, BASH_TOOL};
use harness_core::{Agent, Error, Steering};
use tokio::sync::mpsc;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

use crate::session::{PromptSaver, Session};
use crate::terminal::{EventReader, InlineTerminal};
use crate::{Action, AgentUpdate, App, PromptStats, Signals, UndoPlan};

/// The terminal, what it shows and what the user does there.
pub(crate) struct Ui {
    pub(crate) term: InlineTerminal,
    pub(crate) app: App,
    pub(crate) events: EventReader,
    /// The questions the model asks the user.
    pub(crate) questions: mpsc::UnboundedReceiver<Question>,
    /// What asks to quit from outside.
    pub(crate) signals: Signals,
    /// The user's own `AGENTS.md`.
    pub(crate) agents_file: Option<PathBuf>,
    /// What the question shown asks to confirm, when it is `/undo`.
    pub(crate) undo_asked: Option<UndoPlan>,
}

impl Ui {
    pub(crate) fn draw(&mut self) -> Result<(), Error> {
        self.term.draw_app(&mut self.app)?;
        Ok(())
    }
}

const SPINNER_TICK: Duration = Duration::from_millis(120);

/// Run the shell `command` the user typed after `!`, showing it as a `bash`
/// call. Returns `true` if the user requested to quit.
pub(crate) async fn run_shell(ui: &mut Ui, agent: &Agent, command: &str) -> Result<bool, Error> {
    ui.app.start_activity();
    ui.app.apply_update(AgentUpdate::ToolCall {
        name: BASH_TOOL.to_owned(),
        args: serde_json::json!({ "command": command }).to_string(),
    });
    ui.draw()?;

    let (tx, rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let run = async {
        let content = agent
            .run_command(command, |chunk| {
                let _ = tx.send(AgentUpdate::ToolOutput(chunk.to_owned()));
            })
            .await;
        let _ = tx.send(AgentUpdate::ToolResult {
            content,
            details: None,
        });
        Ok(String::new())
    };
    let res = drive_agent(ui, rx, agent.steering(), run).await?;
    if finish_activity(&mut ui.app, res) {
        return Ok(true);
    }
    restore_queued(agent, &mut ui.app);
    // The command may end before its result was drawn.
    ui.draw()?;
    Ok(false)
}

/// Put messages typed while no prompt ran back in the editor.
pub(crate) fn restore_queued(agent: &Agent, app: &mut App) {
    let queued = agent.steering().take().join("\n\n");
    if !queued.is_empty() {
        app.restore_queued(&queued);
    }
}

struct DriveResult {
    cancelled: bool,
    quit: bool,
    outcome: Option<Result<String, Error>>,
}

pub(crate) struct TurnOutcome {
    pub(crate) quit: bool,
    pub(crate) cancelled: bool,
    pub(crate) changed_files: bool,
}

/// Drive an in-flight agent future (`prompt` or `compact`) while keeping the
/// UI responsive to keys, resize, questions, and streaming updates.
async fn drive_agent(
    ui: &mut Ui,
    mut rx: mpsc::UnboundedReceiver<AgentUpdate>,
    steering: Steering,
    fut: impl std::future::Future<Output = Result<String, Error>>,
) -> Result<DriveResult, Error> {
    let mut cancelled = false;
    let mut quit = false;
    let mut outcome: Option<Result<String, Error>> = None;
    // The question of the model waiting for the user's answer.
    let mut question: Option<Question> = None;

    // Scope `fut` so dropping it on exit runs cancel cleanup and closes `tx`
    // before we drain `rx`.
    {
        tokio::pin!(fut);

        let mut ticker = interval_at(Instant::now() + SPINNER_TICK, SPINNER_TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = ui.signals.recv() => {
                    quit = true;
                    break;
                }
                maybe_ev = ui.events.next() => {
                    match maybe_ev {
                        Some(Ok(ev)) => match handle_event_batch(ev, &mut ui.events, &mut ui.app)? {
                            Action::Cancel => {
                                cancelled = true;
                                break;
                            }
                            Action::Quit => {
                                quit = true;
                                break;
                            }
                            Action::Redraw => ui.draw()?,
                            Action::Submit(text) => {
                                steering.push(text);
                                ui.draw()?;
                            }
                            Action::None
                            | Action::Shell(_)
                            | Action::Slash(_)
                            | Action::ResumeSession(_)
                            | Action::SelectModel(_)
                            | Action::SetPlan(_)
                            | Action::PasteImage => {}
                            Action::Answer(answer) => {
                                if let Some(question) = question.take() {
                                    question.answer(answer);
                                }
                                ui.draw()?;
                            }
                        },
                        Some(Err(err)) => return Err(err.into()),
                        None => {
                            quit = true;
                            break;
                        }
                    }
                }
                Some(update) = rx.recv() => {
                    drain_updates(update, &mut rx, &mut ui.app);
                    ui.draw()?;
                }
                // After `rx`, so the tool call shows before its question.
                Some(asked) = ui.questions.recv() => {
                    ui.app.ask(&asked.text, &asked.options);
                    question = Some(asked);
                    ui.draw()?;
                }
                update = ui.app.next_changes() => {
                    ui.app.apply_changes(update);
                    ui.draw()?;
                }
                res = &mut fut => {
                    outcome = Some(res);
                    break;
                }
                _ = ticker.tick() => {
                    ui.app.tick();
                    ui.draw()?;
                }
            }
        }
    }

    if let Ok(first) = rx.try_recv() {
        drain_updates(first, &mut rx, &mut ui.app);
    }
    // A question sent just before a cancel has nobody waiting for it: drop
    // it so the next prompt does not show it.
    while ui.questions.try_recv().is_ok() {}

    Ok(DriveResult {
        cancelled,
        quit,
        outcome,
    })
}

/// Run one agent prompt while keeping the UI responsive to keys, resize and
/// streaming updates.
pub(crate) async fn run_turn(
    ui: &mut Ui,
    agent: &mut Agent,
    session: Option<&mut Session>,
    prompt: &str,
) -> Result<TurnOutcome, Error> {
    let mut saver = PromptSaver::new(session, agent.history());
    let mut stats = PromptStats::default();
    let (tx, rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let res = drive_agent(
        ui,
        rx,
        agent.steering(),
        agent.prompt(prompt, |event| {
            saver.on_event(&event);
            stats.on_event(&event);
            forward_event(&tx, event);
        }),
    )
    .await?;

    let cancelled = res.cancelled;
    if let Err(err) = saver.finish(agent.history(), stats.usage, stats.context) {
        ui.app.push_info(format!("[session save error: {err}]"));
    }

    let quit = finish_activity(&mut ui.app, res);
    if !quit {
        ui.draw()?;
    }
    Ok(TurnOutcome {
        quit,
        cancelled,
        changed_files: stats.changed_files,
    })
}

/// Summarize the conversation with `/compact`, replacing history on success.
/// Returns `true` if the user requested to quit.
pub(crate) async fn run_compact(
    ui: &mut Ui,
    agent: &mut Agent,
    session: Option<&mut Session>,
    focus: Option<&str>,
) -> Result<bool, Error> {
    let before = agent.history().len();
    ui.app.start_activity();
    ui.app.push_info("[compacting conversation…]");
    ui.draw()?;

    let mut stats = PromptStats::default();
    let (tx, rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let res = drive_agent(
        ui,
        rx,
        agent.steering(),
        agent.compact(focus, |event| {
            stats.on_event(&event);
            forward_event(&tx, event);
        }),
    )
    .await?;

    let compacted = !res.cancelled && !res.quit && matches!(res.outcome, Some(Ok(_)));
    let usage = stats.usage;
    if finish_activity(&mut ui.app, res) {
        return Ok(true);
    }
    // Messages typed meanwhile wait in the editor.
    restore_queued(agent, &mut ui.app);
    if compacted {
        ui.app.set_context(0);
        if let Some(session) = session {
            if let Err(err) = session.record_compact(agent.history(), usage, 0) {
                ui.app.push_info(format!("[session save error: {err}]"));
            }
        }
        ui.app.push_info(format!("[compacted {before} messages]"));
    }
    Ok(false)
}

fn forward_event(tx: &mpsc::UnboundedSender<AgentUpdate>, event: harness_core::Event<'_>) {
    if let Some(update) = AgentUpdate::from_event(event) {
        let _ = tx.send(update);
    }
}

fn finish_activity(app: &mut App, res: DriveResult) -> bool {
    if res.quit {
        return true;
    }
    if res.cancelled {
        app.cancel_prompt();
    } else if let Some(outcome) = res.outcome {
        app.finish_prompt(outcome.map(|_| ()).map_err(|e| e.to_string()));
    }
    false
}

/// Process `first` and any immediately queued terminal events, coalescing
/// consecutive redraws (e.g. window resize drags or fast typing) into a single
/// draw while stopping immediately on control actions.
pub(crate) fn handle_event_batch(
    first: crossterm::event::Event,
    events: &mut EventReader,
    app: &mut App,
) -> Result<Action, Error> {
    let action = app.handle_event(first);
    if !matches!(action, Action::None | Action::Redraw) {
        return Ok(action);
    }
    let mut dirty = matches!(action, Action::Redraw);
    while let Some(queued) = events.try_next() {
        match app.handle_event(queued?) {
            Action::None => {}
            Action::Redraw => dirty = true,
            other => return Ok(other),
        }
    }
    Ok(if dirty { Action::Redraw } else { Action::None })
}

/// Apply `first` and any immediately queued updates from `rx`, coalescing
/// consecutive `TextDelta` or `ReasoningDelta` chunks so Markdown parsing
/// and terminal redraws run once per batch instead of once per token.
fn drain_updates(first: AgentUpdate, rx: &mut mpsc::UnboundedReceiver<AgentUpdate>, app: &mut App) {
    let mut pending = first;
    while let Ok(next) = rx.try_recv() {
        match (&mut pending, next) {
            (AgentUpdate::TextDelta(acc), AgentUpdate::TextDelta(more))
            | (AgentUpdate::ReasoningDelta(acc), AgentUpdate::ReasoningDelta(more)) => {
                acc.push_str(&more);
            }
            (_, next) => {
                app.apply_update(pending);
                pending = next;
            }
        }
    }
    app.apply_update(pending);
}
