// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What the slash commands do.

use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use genai::adapter::AdapterKind;
use genai::chat::ChatMessage;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use harness_core::{export, Agent, Error, ModelInfo, ModelList, Thinking};

use crate::drive::{run_compact, Ui};
use crate::session::{short_id, Session};
use crate::{clipboard, complete, App, ChangeKind, SlashCommand, Undo, UndoPlan};

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

/// The answer that confirms `/undo`.
const UNDO: &str = "Undo";

/// What tells the last of `messages`: a checkpoint of the files goes with
/// the conversation it was taken in, not with one as long.
pub(crate) fn history_tag(messages: &[ChatMessage]) -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Some(last) = messages.last() {
        serde_json::to_string(last)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// Forget the checkpoints of the files that are not of the conversation
/// of `agent` as it is now: the one of a prompt that left no message, as
/// when it failed at once or is sent again, and all of them once the
/// conversation was rewritten, as a compaction does.
pub(crate) async fn sync_checkpoints(app: &App, agent: &Agent) {
    let history = agent.history();
    while let Some((messages, tag)) = app.last_checkpoint().await {
        let intact = messages <= history.len() && history_tag(&history[..messages]) == tag;
        if intact && messages < history.len() {
            return;
        }
        app.forget_checkpoints(!intact);
    }
}

/// What `/undo` would do in the conversation of `agent`.
async fn undo_plan(app: &App, agent: &Agent) -> Option<UndoPlan> {
    sync_checkpoints(app, agent).await;
    app.undo_plan().await
}

/// One line for each file `/undo` handles, as `/diff` names them.
fn undo_lines(plan: &UndoPlan) -> Vec<String> {
    let changed = plan.files.iter().map(|file| {
        let what = match file.kind {
            ChangeKind::Added => "delete",
            ChangeKind::Modified => "restore",
            ChangeKind::Deleted => "create",
        };
        format!("  {what:<8}{}", file.path.display())
    });
    let kept = plan
        .kept
        .iter()
        .map(|(path, reason)| format!("  keep    {} ({reason})", path.display()));
    changed.chain(kept).collect()
}

/// Show what `/undo` would do and ask to confirm it. Returns the plan
/// shown, when the question was asked.
async fn ask_undo(app: &mut App, agent: &Agent) -> Option<UndoPlan> {
    let Some(plan) = undo_plan(app, agent).await else {
        app.push_info("[nothing to undo]");
        return None;
    };
    let question = if plan.files.is_empty() && plan.kept.is_empty() {
        "The last prompt changed no file. Drop it from the conversation?".to_owned()
    } else {
        app.push_info("Files changed since the last prompt was sent:");
        for line in undo_lines(&plan) {
            app.push_info(line);
        }
        "Put these files back as they were, and drop the last prompt from the conversation?"
            .to_owned()
    };
    app.ask(&question, &[UNDO.to_owned(), "Cancel".to_owned()]);
    Some(plan)
}

/// With the `answer` that confirms the plan `shown`, put the files back
/// as they were before the last prompt, and drop the prompt and what it
/// led to. When the files changed since, so that the plan is another one,
/// it is shown and asked about again: returns the plan then shown.
pub(crate) async fn undo(
    app: &mut App,
    answer: &str,
    shown: UndoPlan,
    agent: &mut Agent,
    session: Option<&mut Session>,
) -> Option<UndoPlan> {
    if answer != UNDO {
        app.push_info("[undo cancelled]");
        return None;
    }
    // Looked up again: only a checkpoint of this conversation is used.
    if undo_plan(app, agent).await.is_none() {
        app.push_info("[nothing to undo]");
        return None;
    }
    let Undo { plan, failed } = match app.undo(shown).await {
        Some(Ok(undo)) => undo,
        Some(Err(_)) => {
            app.push_info("[the files changed since: nothing was undone]");
            return ask_undo(app, agent).await;
        }
        None => {
            app.push_info("[nothing to undo]");
            return None;
        }
    };
    let mut history = agent.history().to_vec();
    history.truncate(plan.messages);
    agent.set_history(history);
    if let Some(session) = session {
        if let Err(err) = session.record_rewind(agent.history()) {
            app.push_info(format!("[session save error: {err}]"));
        }
    }
    for (path, err) in &failed {
        app.push_info(format!("[undo error: {}: {err}]", path.display()));
    }
    let files = plan.files.len() - failed.len();
    let unit = if files == 1 { "file" } else { "files" };
    app.push_info(format!(
        "[undone: {files} {unit} put back, last prompt dropped]"
    ));
    None
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
        SlashCommand::Undo => ui.undo_asked = ask_undo(&mut ui.app, agent).await,
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
    // The files as they were before the prompts of another conversation.
    app.forget_checkpoints(true);
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
