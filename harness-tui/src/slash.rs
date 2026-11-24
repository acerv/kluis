// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What the slash commands do.

use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use genai::adapter::AdapterKind;
use harness_core::tools::Question;
use harness_core::{Agent, Error, ModelInfo, ModelList, Thinking};
use tokio::sync::mpsc;

use crate::drive::{draw, run_compact};
use crate::session::{short_id, Session};
use crate::terminal::{EventReader, InlineTerminal};
use crate::{clipboard, complete, App, SlashCommand};

/// Take the last prompt out of the conversation, to send it again.
pub(crate) fn retry(
    agent: &mut Agent,
    app: &mut App,
    session: Option<&mut Session>,
) -> Option<String> {
    let Some(prompt) = agent.take_last_prompt() else {
        app.push_info("[nothing to retry]");
        return None;
    };
    if let Some(session) = session {
        if let Err(err) = session.record_rewind(agent.history()) {
            app.push_info(format!("[session save error: {err}]"));
        }
    }
    app.push_info("[retrying the last prompt]");
    Some(prompt)
}

/// The models the user may pick, asked to the providers once.
pub(crate) struct ModelCatalog {
    adapters: Vec<AdapterKind>,
    cached: Option<Vec<ModelInfo>>,
}

impl ModelCatalog {
    pub(crate) fn new(adapters: Vec<AdapterKind>) -> Self {
        Self {
            adapters,
            cached: None,
        }
    }
}

/// Execute a slash command, setting `next_prompt` to a prompt to send.
/// Returns `true` when the user requested to quit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_slash(
    cmd: SlashCommand,
    term: &mut InlineTerminal,
    agent: &mut Agent,
    app: &mut App,
    events: &mut EventReader,
    questions: &mut mpsc::UnboundedReceiver<Question>,
    session: Option<&mut Session>,
    models: &mut ModelCatalog,
    next_prompt: &mut Option<String>,
) -> Result<bool, Error> {
    match cmd {
        SlashCommand::Quit => return Ok(true),
        SlashCommand::Retry => *next_prompt = retry(agent, app, session),
        SlashCommand::Compact(focus) => {
            if agent.history().is_empty() {
                app.push_info("[nothing to compact]");
                return Ok(false);
            }
            return run_compact(
                term,
                app,
                agent,
                events,
                questions,
                session,
                focus.as_deref(),
            )
            .await;
        }
        SlashCommand::Status => {
            let messages = agent.history().len();
            let label = session
                .as_deref()
                .map(|s| if messages == 0 { "new" } else { s.short_id() });
            app.push_info(app.status_text(label, messages));
        }
        SlashCommand::Output => app.show_tool_output(),
        SlashCommand::Diff => app.toggle_diff(),
        SlashCommand::Copy => match clipboard::last_assistant_text(agent.history()) {
            None => app.push_info("[nothing to copy]"),
            Some(text) => match clipboard::copy(&text) {
                Ok(()) => {
                    let chars = text.chars().count();
                    let unit = if chars == 1 { "char" } else { "chars" };
                    app.push_info(format!("[copied last reply ({chars} {unit})]"));
                }
                Err(err) => app.push_info(format!("[copy error: {err}]")),
            },
        },
        SlashCommand::New => {
            agent.clear();
            app.reset_session();
            if let Some(session) = session {
                session.rotate(agent.model());
            }
            term.request_clear();
            app.push_info("[new session]");
        }
        SlashCommand::Model(Some(name)) => {
            switch_model(&name, agent, app, session);
        }
        SlashCommand::Model(None) => {
            return open_model_picker(term, agent, app, events, models).await;
        }
        SlashCommand::Thinking(Some(arg)) => match arg.parse::<Thinking>() {
            Ok(level) => match app.thinking_levels().filter(|l| !l.contains(&level)) {
                Some(levels) => {
                    let takes = level_list(levels);
                    app.push_info(format!(
                        "[{} does not take thinking {level}; it takes {takes}]",
                        agent.model()
                    ));
                }
                None => {
                    agent.set_thinking(level);
                    app.set_thinking(level);
                    app.push_info(format!("thinking: {level}"));
                }
            },
            Err(err) => app.push_info(err),
        },
        SlashCommand::Thinking(None) => {
            let takes = app
                .thinking_levels()
                .map(|l| format!(" (this model takes {})", level_list(l)))
                .unwrap_or_default();
            app.push_info(format!("thinking: {}{takes}", agent.thinking()));
        }
        SlashCommand::Resume(Some(id)) => {
            resume_session(&id, None, agent, app, session);
        }
        SlashCommand::Resume(None) => match session {
            None => app.push_info("[sessions disabled (--no-session)]"),
            Some(session) => match session.list() {
                Ok(entries) if entries.is_empty() => {
                    app.push_info("[no saved sessions for this folder]");
                }
                Ok(entries) => {
                    let current =
                        (!agent.history().is_empty()).then(|| session.current_id().to_owned());
                    app.open_picker(entries, current);
                }
                Err(err) => app.push_info(format!("[resume error: {err}]")),
            },
        },
        SlashCommand::Help => {
            app.push_info(complete::help_text());
        }
    }
    Ok(false)
}

/// Ask which thinking levels the model takes, and fall back to the
/// default level when it does not take the current one.
pub(crate) async fn check_thinking(agent: &mut Agent, app: &mut App) {
    let levels = agent.thinking_levels().await;
    let thinking = agent.thinking();
    if levels.as_ref().is_some_and(|l| !l.contains(&thinking)) {
        app.push_info(format!(
            "[{} does not take thinking {thinking}; using default]",
            agent.model()
        ));
        agent.set_thinking(Thinking::Default);
        app.set_thinking(Thinking::Default);
    }
    app.set_thinking_levels(levels);
}

/// `levels` as a comma-separated list.
fn level_list(levels: &[Thinking]) -> String {
    levels
        .iter()
        .map(|level| level.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn switch_model(
    name: &str,
    agent: &mut Agent,
    app: &mut App,
    session: Option<&mut Session>,
) {
    if name != agent.model() {
        agent.set_model(name);
        app.set_model(name);
        if let Some(session) = session {
            if let Err(err) = session.record_model(name) {
                app.push_info(format!("[session save error: {err}]"));
            }
        }
    }
    app.push_info(format!("model: {name}"));
}

enum FetchOutcome {
    Done(ModelList),
    Cancelled,
    Quit,
}

async fn open_model_picker(
    term: &mut InlineTerminal,
    agent: &Agent,
    app: &mut App,
    events: &mut EventReader,
    models: &mut ModelCatalog,
) -> Result<bool, Error> {
    if models.adapters.is_empty() {
        app.push_info(format!("model: {}", agent.model()));
        app.push_info(
            "[no configured providers — set an API key env var or ~/.kluis/auth.toml, \
             or run `kl --login <provider>`]",
        );
        return Ok(false);
    }

    if models.cached.is_none() {
        app.push_info("[loading models…]");
        draw(term, app)?;
        match fetch_models(term, agent, app, events, &models.adapters).await? {
            FetchOutcome::Quit => return Ok(true),
            FetchOutcome::Cancelled => {
                app.push_info("[cancelled]");
                return Ok(false);
            }
            FetchOutcome::Done(list) => {
                for (provider, err) in list.errors {
                    app.push_info(format!("[models error ({provider}): {err}]"));
                }
                if !list.models.is_empty() {
                    models.cached = Some(list.models);
                }
            }
        }
    }

    if let Some(list) = models.cached.as_deref() {
        app.open_model_picker(list);
    } else {
        app.push_info(format!("model: {}", agent.model()));
        app.push_info("[no models returned by configured providers]");
    }
    Ok(false)
}

async fn fetch_models(
    term: &mut InlineTerminal,
    agent: &Agent,
    app: &mut App,
    events: &mut EventReader,
    adapters: &[AdapterKind],
) -> Result<FetchOutcome, Error> {
    let fut = agent.list_models(adapters);
    tokio::pin!(fut);

    loop {
        tokio::select! {
            biased;
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(TermEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match (key.code, ctrl) {
                        (KeyCode::Esc, false) | (KeyCode::Char('c'), true) => {
                            return Ok(FetchOutcome::Cancelled);
                        }
                        (KeyCode::Char('d'), true) => return Ok(FetchOutcome::Quit),
                        _ => {}
                    }
                }
                Some(Ok(ev @ TermEvent::Resize(_, _))) => {
                    app.handle_event(ev);
                    draw(term, app)?;
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(err.into()),
                None => return Ok(FetchOutcome::Quit),
            },
            list = &mut fut => return Ok(FetchOutcome::Done(list)),
        }
    }
}

/// Switch to the session `id`, with its own model unless `model` is given.
pub(crate) fn resume_session(
    id: &str,
    model: Option<String>,
    agent: &mut Agent,
    app: &mut App,
    session: Option<&mut Session>,
) {
    let Some(session) = session else {
        app.push_info("[sessions disabled (--no-session)]");
        return;
    };
    let data = match session.load(id) {
        Ok(data) => data,
        Err(err) => {
            app.push_info(format!("[resume error: {err}]"));
            return;
        }
    };
    if data.meta.id == session.current_id() && !agent.history().is_empty() {
        app.push_info("[already in this session]");
        return;
    }
    session.activate(&data);
    let model = model.unwrap_or_else(|| data.meta.model.clone());
    if model != data.meta.model {
        if let Err(err) = session.record_model(&model) {
            app.push_info(format!("[session save error: {err}]"));
        }
    }
    agent.set_model(&model);
    app.set_model(&model);
    agent.set_history(data.messages);
    let short = short_id(&data.meta.id);
    app.push_info(format!(
        "[resumed session {short} · {} messages]",
        agent.history().len()
    ));
    app.replay_history(agent.history());
    app.set_usage(session.usage());
    app.set_context(session.context());
}
