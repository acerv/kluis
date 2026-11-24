// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Running the agent while the terminal stays live.

use std::time::Duration;

use harness_core::tools::{Question, BASH_TOOL};
use harness_core::{Agent, Error, Steering};
use tokio::sync::mpsc;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

use crate::session::{PromptSaver, Session};
use crate::terminal::{EventReader, InlineTerminal};
use crate::{Action, AgentUpdate, App, PromptStats};

const SPINNER_TICK: Duration = Duration::from_millis(120);

/// Run the shell `command` the user typed after `!`, showing it as a `bash`
/// call. Returns `true` if the user requested to quit.
pub(crate) async fn run_shell(
    term: &mut InlineTerminal,
    app: &mut App,
    agent: &Agent,
    events: &mut EventReader,
    questions: &mut mpsc::UnboundedReceiver<Question>,
    command: &str,
) -> Result<bool, Error> {
    app.start_activity();
    app.apply_update(AgentUpdate::ToolCall {
        name: BASH_TOOL.to_owned(),
        args: serde_json::json!({ "command": command }).to_string(),
    });
    draw(term, app)?;

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
    let res = drive_agent(term, app, events, questions, rx, agent.steering(), run).await?;
    if finish_activity(app, res) {
        return Ok(true);
    }
    restore_queued(agent, app);
    // The command may end before its result was drawn.
    draw(term, app)?;
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
    term: &mut InlineTerminal,
    app: &mut App,
    events: &mut EventReader,
    questions: &mut mpsc::UnboundedReceiver<Question>,
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
                maybe_ev = events.next() => {
                    match maybe_ev {
                        Some(Ok(ev)) => match handle_event_batch(ev, events, app)? {
                            Action::Cancel => {
                                cancelled = true;
                                break;
                            }
                            Action::Quit => {
                                quit = true;
                                break;
                            }
                            Action::Redraw => draw(term, app)?,
                            Action::Submit(text) => {
                                steering.push(text);
                                draw(term, app)?;
                            }
                            Action::None
                            | Action::Shell(_)
                            | Action::Slash(_)
                            | Action::ResumeSession(_)
                            | Action::SelectModel(_)
                            | Action::SetPlan(_) => {}
                            Action::Answer(answer) => {
                                if let Some(question) = question.take() {
                                    question.answer(answer);
                                }
                                draw(term, app)?;
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
                    drain_updates(update, &mut rx, app);
                    draw(term, app)?;
                }
                // After `rx`, so the tool call shows before its question.
                Some(asked) = questions.recv() => {
                    app.ask(&asked.text, &asked.options);
                    question = Some(asked);
                    draw(term, app)?;
                }
                update = app.next_changes() => {
                    app.apply_changes(update);
                    draw(term, app)?;
                }
                res = &mut fut => {
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

    if let Ok(first) = rx.try_recv() {
        drain_updates(first, &mut rx, app);
    }
    // A question sent just before a cancel has nobody waiting for it: drop
    // it so the next prompt does not show it.
    while questions.try_recv().is_ok() {}

    Ok(DriveResult {
        cancelled,
        quit,
        outcome,
    })
}

/// Run one agent prompt while keeping the UI responsive to keys, resize and
/// streaming updates.
pub(crate) async fn run_turn(
    term: &mut InlineTerminal,
    app: &mut App,
    agent: &mut Agent,
    events: &mut EventReader,
    questions: &mut mpsc::UnboundedReceiver<Question>,
    session: Option<&mut Session>,
    prompt: &str,
) -> Result<TurnOutcome, Error> {
    let mut saver = PromptSaver::new(session, agent.history());
    let mut stats = PromptStats::default();
    let (tx, rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let res = drive_agent(
        term,
        app,
        events,
        questions,
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
        app.push_info(format!("[session save error: {err}]"));
    }

    let quit = finish_activity(app, res);
    if !quit {
        draw(term, app)?;
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
    term: &mut InlineTerminal,
    app: &mut App,
    agent: &mut Agent,
    events: &mut EventReader,
    questions: &mut mpsc::UnboundedReceiver<Question>,
    session: Option<&mut Session>,
    focus: Option<&str>,
) -> Result<bool, Error> {
    let before = agent.history().len();
    app.start_activity();
    app.push_info("[compacting conversation…]");
    draw(term, app)?;

    let mut stats = PromptStats::default();
    let (tx, rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let res = drive_agent(
        term,
        app,
        events,
        questions,
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
    if finish_activity(app, res) {
        return Ok(true);
    }
    // Messages typed meanwhile wait in the editor.
    restore_queued(agent, app);
    if compacted {
        app.set_context(0);
        if let Some(session) = session {
            if let Err(err) = session.record_compact(agent.history(), usage, 0) {
                app.push_info(format!("[session save error: {err}]"));
            }
        }
        app.push_info(format!("[compacted {before} messages]"));
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

pub(crate) fn draw(term: &mut InlineTerminal, app: &mut App) -> Result<(), Error> {
    term.draw_app(app)?;
    Ok(())
}
