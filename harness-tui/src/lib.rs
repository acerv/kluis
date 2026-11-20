// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Inline terminal user interface for LLM harnesses, keeping the native
//! terminal scrollback.
//!
//! Only Linux is supported.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-tui only supports Linux");

mod app;
mod clipboard;
mod complete;
mod history;
mod input;
mod markdown;
mod picker;
mod style;
mod terminal;

use std::io;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use genai::adapter::AdapterKind;
use genai::chat::ChatMessage;
use harness_core::{
    Agent, Error, ModelInfo, ModelList, SessionData, SessionStore, SessionSummary, SessionWriter,
    Thinking, TokenUsage,
};
use tokio::sync::mpsc;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

pub use app::{summary, Action, AgentUpdate, App, SlashCommand, VIEWPORT_HEIGHT};
pub use clipboard::{last_assistant_text, osc52_sequence};
pub use history::{History, MAX_HISTORY};
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
}

/// Active persistence handle for an interactive or single-shot session.
pub struct Session {
    store: SessionStore,
    writer: SessionWriter,
    initial_tokens: TokenUsage,
}

impl Session {
    pub fn new(store: SessionStore, writer: SessionWriter, initial_tokens: TokenUsage) -> Self {
        Self {
            store,
            writer,
            initial_tokens,
        }
    }

    /// Persist messages added during a prompt and any accumulated token usage.
    pub fn record_prompt(&mut self, messages: &[ChatMessage], usage: TokenUsage) -> io::Result<()> {
        self.writer.append_messages(messages)?;
        self.writer.append_usage(usage)
    }

    /// Persist a model change.
    pub fn record_model(&mut self, model: &str) -> io::Result<()> {
        self.writer.append_model(model)
    }

    pub fn current_id(&self) -> &str {
        self.writer.id()
    }

    /// List persisted sessions for the current workspace, newest first.
    pub fn list(&self) -> Result<Vec<SessionSummary>, Error> {
        self.store.list()
    }

    /// Load a persisted session by exact ID or unique prefix.
    pub fn load(&self, id: &str) -> Result<SessionData, Error> {
        self.store.load(id)
    }

    /// Switch the active writer to an already-loaded session so further turns
    /// append to it.
    pub fn activate(&mut self, data: &SessionData) {
        self.writer = self.store.open(data);
        self.initial_tokens = data.usage;
    }

    /// Start a fresh lazy session file (e.g. after `/new`), leaving the
    /// previous session file intact on disk.
    pub fn rotate(&mut self, model: &str) {
        self.writer = self.store.create(model);
        self.initial_tokens = TokenUsage::default();
    }
}

/// Optional state passed into [`run`].
#[derive(Default)]
pub struct RunOptions {
    pub session: Option<Session>,
    pub history: Option<History>,
    pub picker_entries: Vec<SessionSummary>,
    pub adapters: Vec<AdapterKind>,
}

/// Run the interactive inline TUI until the user quits (`Ctrl-D` or `/quit`).
pub async fn run(mut agent: Agent, status: Status, opts: RunOptions) -> Result<(), Error> {
    let RunOptions {
        mut session,
        mut history,
        picker_entries,
        adapters,
    } = opts;
    let mut models = ModelCatalog {
        adapters,
        cached: None,
    };
    let mut term = InlineTerminal::enter(VIEWPORT_HEIGHT)?;
    let mut app = App::new(agent.model().to_owned(), status, Theme::from_env());
    app.set_thinking(agent.thinking());
    if let Some(ref h) = history {
        app.set_history(h.entries());
    }
    if let Some(ref s) = session {
        app.set_tokens(s.initial_tokens);
    }
    if !agent.history().is_empty() {
        app.push_info(format!(
            "[continued session · {} messages]",
            agent.history().len()
        ));
        app.replay_history(agent.history());
    } else if !picker_entries.is_empty() {
        app.open_picker(picker_entries, None);
    }
    let mut events = EventReader::spawn();

    draw(&mut term, &mut app)?;

    loop {
        let Some(next) = events.next().await else {
            break;
        };
        match handle_event_batch(next?, &mut events, &mut app)? {
            Action::None | Action::Cancel => {}
            Action::Redraw => draw(&mut term, &mut app)?,
            Action::Quit => break,
            Action::Slash(cmd) => {
                if handle_slash(
                    cmd,
                    &mut term,
                    &mut agent,
                    &mut app,
                    &mut events,
                    session.as_mut(),
                    &mut models,
                )
                .await?
                {
                    break;
                }
                draw(&mut term, &mut app)?;
            }
            Action::ResumeSession(id) => {
                resume_session(&id, &mut agent, &mut app, session.as_mut());
                draw(&mut term, &mut app)?;
            }
            Action::SelectModel(name) => {
                switch_model(&name, &mut agent, &mut app, session.as_mut());
                draw(&mut term, &mut app)?;
            }
            Action::SetThinking(level) => {
                agent.set_thinking(level);
                draw(&mut term, &mut app)?;
            }
            Action::Submit(prompt) => {
                if let Some(ref mut h) = history {
                    let _ = h.record(&prompt);
                }
                app.start_prompt(&prompt);
                draw(&mut term, &mut app)?;
                if run_turn(
                    &mut term,
                    &mut app,
                    &mut agent,
                    &mut events,
                    session.as_mut(),
                    &prompt,
                )
                .await?
                {
                    break;
                }
            }
        }
    }

    Ok(())
}

struct ModelCatalog {
    adapters: Vec<AdapterKind>,
    cached: Option<Vec<ModelInfo>>,
}

/// Execute a slash command. Returns `true` when the user requested to quit.
async fn handle_slash(
    cmd: SlashCommand,
    term: &mut InlineTerminal,
    agent: &mut Agent,
    app: &mut App,
    events: &mut EventReader,
    session: Option<&mut Session>,
    models: &mut ModelCatalog,
) -> Result<bool, Error> {
    match cmd {
        SlashCommand::Quit => return Ok(true),
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
            Ok(level) => {
                agent.set_thinking(level);
                app.set_thinking(level);
                app.push_info(format!("thinking: {level}"));
            }
            Err(err) => app.push_info(err),
        },
        SlashCommand::Thinking(None) => {
            app.push_info(format!("thinking: {}", agent.thinking()));
        }
        SlashCommand::Resume(Some(id)) => {
            resume_session(&id, agent, app, session);
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

fn switch_model(name: &str, agent: &mut Agent, app: &mut App, session: Option<&mut Session>) {
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

fn resume_session(id: &str, agent: &mut Agent, app: &mut App, session: Option<&mut Session>) {
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
    agent.set_model(&data.meta.model);
    app.set_model(&data.meta.model);
    app.set_tokens(data.usage);
    agent.set_history(data.messages);
    let short = data.meta.id.get(..15).unwrap_or(&data.meta.id);
    app.push_info(format!(
        "[resumed session {short} · {} messages]",
        agent.history().len()
    ));
    app.replay_history(agent.history());
}

/// Run one agent prompt while keeping the UI responsive to keys, resize and
/// streaming updates. Returns `true` if the user requested to quit.
async fn run_turn(
    term: &mut InlineTerminal,
    app: &mut App,
    agent: &mut Agent,
    events: &mut EventReader,
    session: Option<&mut Session>,
    prompt: &str,
) -> Result<bool, Error> {
    let prev_len = agent.history().len();
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let mut cancelled = false;
    let mut quit = false;
    let mut outcome: Option<Result<String, Error>> = None;
    let mut prompt_usage = TokenUsage::default();

    // Scope `prompt_fut` so dropping it on exit runs `Agent::prompt`'s cancel
    // cleanup and closes `tx` before we drain `rx`.
    {
        let prompt_fut = agent.prompt(prompt, |event| {
            if let Some(update) = AgentUpdate::from_event(event) {
                let _ = tx.send(update);
            }
        });
        tokio::pin!(prompt_fut);

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
                            Action::None
                            | Action::Submit(_)
                            | Action::Slash(_)
                            | Action::ResumeSession(_)
                            | Action::SelectModel(_)
                            | Action::SetThinking(_) => {}
                        },
                        Some(Err(err)) => return Err(err.into()),
                        None => {
                            quit = true;
                            break;
                        }
                    }
                }
                Some(update) = rx.recv() => {
                    drain_updates(update, &mut rx, app, &mut prompt_usage);
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

    if let Ok(first) = rx.try_recv() {
        drain_updates(first, &mut rx, app, &mut prompt_usage);
    }

    if let Some(session) = session {
        let added = &agent.history()[prev_len..];
        if let Err(err) = session.record_prompt(added, prompt_usage) {
            app.push_info(format!("[session save error: {err}]"));
        }
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

/// Process `first` and any immediately queued terminal events, coalescing
/// consecutive redraws (e.g. window resize drags or fast typing) into a single
/// draw while stopping immediately on control actions.
fn handle_event_batch(
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
fn drain_updates(
    first: AgentUpdate,
    rx: &mut mpsc::UnboundedReceiver<AgentUpdate>,
    app: &mut App,
    prompt_usage: &mut TokenUsage,
) {
    let mut pending = first;
    while let Ok(next) = rx.try_recv() {
        match (&mut pending, next) {
            (AgentUpdate::TextDelta(acc), AgentUpdate::TextDelta(more))
            | (AgentUpdate::ReasoningDelta(acc), AgentUpdate::ReasoningDelta(more)) => {
                acc.push_str(&more);
            }
            (_, next) => {
                apply_and_track(pending, app, prompt_usage);
                pending = next;
            }
        }
    }
    apply_and_track(pending, app, prompt_usage);
}

fn apply_and_track(update: AgentUpdate, app: &mut App, prompt_usage: &mut TokenUsage) {
    if let AgentUpdate::TurnEnd(usage) = update {
        *prompt_usage += usage;
    }
    app.apply_update(update);
}

fn draw(term: &mut InlineTerminal, app: &mut App) -> Result<(), Error> {
    term.draw_app(app)?;
    Ok(())
}
