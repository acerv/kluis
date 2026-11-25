// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What the slash commands do.

use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use genai::adapter::AdapterKind;
use std::path::{Path, PathBuf};

use harness_core::{export, Agent, Error, ModelInfo, ModelList, Thinking};

use crate::drive::{run_compact, Ui};
use crate::session::{short_id, Session};
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

/// Write the conversation as Markdown to `path`, by default to a file of
/// the working folder named after the session.
fn export(path: Option<String>, agent: &Agent, app: &mut App, session: Option<&Session>) {
    if agent.history().is_empty() {
        app.push_info("[nothing to export]");
        return;
    }
    let path = path.map_or_else(
        || {
            let name = session.map_or("conversation", Session::short_id);
            PathBuf::from(format!("kl-{name}.md"))
        },
        PathBuf::from,
    );
    match export::write_markdown(&path, agent.history()) {
        Ok(()) => app.push_info(format!("[exported to {}]", path.display())),
        Err(err) => app.push_info(format!("[export error: {}: {err}]", path.display())),
    }
}

/// Add the instruction `arg` to the `AGENTS.md` of the project, or to the
/// user's own (`own`) when it starts with `--global`, and to this
/// conversation.
fn remember(arg: Option<String>, own: Option<&Path>, agent: &mut Agent, app: &mut App) {
    let arg = arg.unwrap_or_default();
    let (global, text) = match arg.strip_prefix("--global") {
        Some(text) if text.is_empty() || text.starts_with(char::is_whitespace) => (true, text),
        _ => (false, arg.as_str()),
    };
    if text.trim().is_empty() {
        app.push_info("usage: /remember [--global] <text>");
        return;
    }
    let path = if global {
        // Only the user writes there: their symlink is followed.
        own.map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()))
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| harness_core::project_agents_file(&cwd))
    };
    let Some(path) = path else {
        app.push_info("[remember error: no folder to write AGENTS.md in]");
        return;
    };
    let saved = match path.parent() {
        Some(dir) if global => harness_core::private_file::create_dir(dir),
        _ => Ok(()),
    }
    .and_then(|()| harness_core::remember(&path, text));
    match saved {
        Ok(line) => {
            agent.add_instruction(&line);
            app.push_info(format!("[added to {}]", path.display()));
        }
        Err(err) => app.push_info(format!("[remember error: {}: {err}]", path.display())),
    }
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
pub(crate) async fn handle_slash(
    ui: &mut Ui,
    cmd: SlashCommand,
    agent: &mut Agent,
    session: Option<&mut Session>,
    models: &mut ModelCatalog,
    next_prompt: &mut Option<String>,
) -> Result<bool, Error> {
    match cmd {
        SlashCommand::Quit => return Ok(true),
        SlashCommand::Retry => *next_prompt = retry(agent, &mut ui.app, session),
        SlashCommand::Compact(focus) => {
            if agent.history().is_empty() {
                ui.app.push_info("[nothing to compact]");
                return Ok(false);
            }
            return run_compact(ui, agent, session, focus.as_deref()).await;
        }
        SlashCommand::Status => {
            let messages = agent.history().len();
            let label = session
                .as_deref()
                .map(|s| if messages == 0 { "new" } else { s.short_id() });
            ui.app.push_info(ui.app.status_text(label, messages));
        }
        SlashCommand::Output => ui.app.show_tool_output(),
        SlashCommand::Diff => ui.app.toggle_diff(),
        SlashCommand::Copy => match clipboard::last_assistant_text(agent.history()) {
            None => ui.app.push_info("[nothing to copy]"),
            Some(text) => match clipboard::copy(&text) {
                Ok(()) => {
                    let chars = text.chars().count();
                    let unit = if chars == 1 { "char" } else { "chars" };
                    ui.app
                        .push_info(format!("[copied last reply ({chars} {unit})]"));
                }
                Err(err) => ui.app.push_info(format!("[copy error: {err}]")),
            },
        },
        SlashCommand::Export(path) => export(path, agent, &mut ui.app, session.as_deref()),
        SlashCommand::Remember(arg) => remember(arg, ui.agents_file.as_deref(), agent, &mut ui.app),
        SlashCommand::Fork => match session {
            None => ui.app.push_info("[sessions disabled (--no-session)]"),
            Some(_) if agent.history().is_empty() => ui.app.push_info("[nothing to fork]"),
            Some(session) => {
                let from = session.short_id().to_owned();
                match session.fork(agent.model(), agent.history()) {
                    Ok(()) => ui.app.push_info(format!(
                        "[forked session {from}: now in {}, {from} stays as it was]",
                        session.short_id()
                    )),
                    Err(err) => ui.app.push_info(format!("[fork error: {err}]")),
                }
            }
        },
        SlashCommand::New => {
            agent.clear();
            ui.app.reset_session();
            if let Some(session) = session {
                session.rotate(agent.model());
            }
            ui.term.request_clear();
            ui.app.push_info("[new session]");
        }
        SlashCommand::Model(Some(name)) => {
            switch_model(&name, agent, &mut ui.app, session);
        }
        SlashCommand::Model(None) => {
            return open_model_picker(ui, agent, models).await;
        }
        SlashCommand::Thinking(Some(arg)) => match arg.parse::<Thinking>() {
            Ok(level) => match ui.app.thinking_levels().filter(|l| !l.contains(&level)) {
                Some(levels) => {
                    let takes = level_list(levels);
                    ui.app.push_info(format!(
                        "[{} does not take thinking {level}; it takes {takes}]",
                        agent.model()
                    ));
                }
                None => {
                    agent.set_thinking(level);
                    ui.app.set_thinking(level);
                    ui.app.push_info(format!("thinking: {level}"));
                }
            },
            Err(err) => ui.app.push_info(err),
        },
        SlashCommand::Thinking(None) => {
            let takes = ui
                .app
                .thinking_levels()
                .map(|l| format!(" (this model takes {})", level_list(l)))
                .unwrap_or_default();
            ui.app
                .push_info(format!("thinking: {}{takes}", agent.thinking()));
        }
        SlashCommand::Resume(Some(id)) => {
            resume_session(&id, None, false, agent, &mut ui.app, session);
        }
        SlashCommand::Resume(None) => match session {
            None => ui.app.push_info("[sessions disabled (--no-session)]"),
            Some(session) => match session.list() {
                Ok(entries) if entries.is_empty() => {
                    ui.app.push_info("[no saved sessions for this folder]");
                }
                Ok(entries) => {
                    let current =
                        (!agent.history().is_empty()).then(|| session.current_id().to_owned());
                    ui.app.open_picker(entries, current);
                }
                Err(err) => ui.app.push_info(format!("[resume error: {err}]")),
            },
        },
        SlashCommand::Help => {
            ui.app.push_info(complete::help_text());
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
    ui: &mut Ui,
    agent: &Agent,
    models: &mut ModelCatalog,
) -> Result<bool, Error> {
    if models.adapters.is_empty() {
        ui.app.push_info(format!("model: {}", agent.model()));
        ui.app.push_info(
            "[no configured providers — set an API key env var or ~/.kluis/auth.toml, \
             or run `kl --login <provider>`]",
        );
        return Ok(false);
    }

    if models.cached.is_none() {
        ui.app.push_info("[loading models…]");
        ui.draw()?;
        match fetch_models(ui, agent, &models.adapters).await? {
            FetchOutcome::Quit => return Ok(true),
            FetchOutcome::Cancelled => {
                ui.app.push_info("[cancelled]");
                return Ok(false);
            }
            FetchOutcome::Done(list) => {
                for (provider, err) in list.errors {
                    ui.app
                        .push_info(format!("[models error ({provider}): {err}]"));
                }
                if !list.models.is_empty() {
                    models.cached = Some(list.models);
                }
            }
        }
    }

    if let Some(list) = models.cached.as_deref() {
        ui.app.open_model_picker(list);
    } else {
        ui.app.push_info(format!("model: {}", agent.model()));
        ui.app
            .push_info("[no models returned by configured providers]");
    }
    Ok(false)
}

async fn fetch_models(
    ui: &mut Ui,
    agent: &Agent,
    adapters: &[AdapterKind],
) -> Result<FetchOutcome, Error> {
    let fut = agent.list_models(adapters);
    tokio::pin!(fut);

    loop {
        tokio::select! {
            biased;
            maybe_ev = ui.events.next() => match maybe_ev {
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
                    ui.app.handle_event(ev);
                    ui.draw()?;
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(err.into()),
                None => return Ok(FetchOutcome::Quit),
            },
            list = &mut fut => return Ok(FetchOutcome::Done(list)),
        }
    }
}

/// Switch to the session `id`, with its own model unless `model` is given,
/// or with `fork` to a copy of it.
pub(crate) fn resume_session(
    id: &str,
    model: Option<String>,
    fork: bool,
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
    let saved = if fork {
        session.fork(&model, &data.messages)
    } else if model != data.meta.model {
        session.record_model(&model)
    } else {
        Ok(())
    };
    if let Err(err) = saved {
        app.push_info(format!("[session save error: {err}]"));
    }
    agent.set_model(&model);
    app.set_model(&model);
    agent.set_history(data.messages);
    let short = short_id(&data.meta.id);
    let what = if fork { "forked" } else { "resumed" };
    app.push_info(format!(
        "[{what} session {short} · {} messages]",
        agent.history().len()
    ));
    app.replay_history(agent.history());
    app.set_usage(session.usage());
    app.set_context(session.context());
}
