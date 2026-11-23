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
use harness_core::tools::{is_read_only, Question};
use harness_core::{
    Agent, Error, Event, ModelInfo, ModelList, SessionData, SessionStore, SessionSummary,
    SessionWriter, Thinking, TokenUsage,
};
use harness_sandbox::GitGuard;
use tokio::sync::mpsc;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

pub use app::{
    printable, pruned_notice, retry_notice, summary, Action, AgentUpdate, App, BannerLine,
    SlashCommand, TRUNCATED_NOTICE, VIEWPORT_HEIGHT,
};
pub use clipboard::{last_assistant_text, osc52_sequence};
pub use history::{History, MAX_HISTORY};
pub use markdown::code_theme;
pub use style::Theme;
pub use terminal::flush_scrollback;
pub use tui_markdown::CodeTheme;

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
    /// Cumulative token usage recorded in this session.
    usage: TokenUsage,
    /// Tokens the last request of the conversation sent, 0 when unknown.
    context: u64,
}

impl Session {
    /// A session with cumulative `usage` and last request size `context` (0
    /// when unknown).
    pub fn new(
        store: SessionStore,
        writer: SessionWriter,
        usage: TokenUsage,
        context: u64,
    ) -> Self {
        Self {
            store,
            writer,
            usage,
            context,
        }
    }

    /// Persist messages added during a prompt, its token usage, and the
    /// tokens its last request sent, 0 when unknown.
    pub fn record_prompt(
        &mut self,
        messages: &[ChatMessage],
        usage: TokenUsage,
        context: u64,
    ) -> io::Result<()> {
        self.usage += usage;
        if context > 0 {
            self.context = context;
        }
        self.writer.append_messages(messages)?;
        self.writer.append_usage(usage, context)
    }

    /// Persist a compacted conversation, the token usage spent during the
    /// prompt or `/compact`, and the latest context size (`0` when reset).
    pub fn record_compact(
        &mut self,
        messages: &[ChatMessage],
        usage: TokenUsage,
        context: u64,
    ) -> io::Result<()> {
        self.usage += usage;
        self.context = context;
        self.writer.append_compact(messages)?;
        self.writer.append_usage(usage, context)
    }

    /// Cumulative token usage recorded in this session.
    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    /// Tokens the last request of the conversation sent, 0 when unknown.
    pub fn context(&self) -> u64 {
        self.context
    }

    /// Persist a model change.
    pub fn record_model(&mut self, model: &str) -> io::Result<()> {
        self.writer.append_model(model)
    }

    pub fn current_id(&self) -> &str {
        self.writer.id()
    }

    /// The start of [`Self::current_id`], enough to tell sessions apart.
    pub fn short_id(&self) -> &str {
        short_id(self.current_id())
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
        self.usage = data.usage;
        self.context = data.context;
    }

    /// Start a fresh lazy session file (e.g. after `/new`), leaving the
    /// previous session file intact on disk.
    pub fn rotate(&mut self, model: &str) {
        self.writer = self.store.create(model);
        self.usage = TokenUsage::default();
        self.context = 0;
    }
}

/// Saves a prompt to its session while it runs, so that one stopped half
/// way, even by a signal, loses the turn it was in at most.
pub struct PromptSaver<'a> {
    session: Option<&'a mut Session>,
    /// Messages of the history that are saved.
    saved: usize,
    /// The first failure, reported when the prompt ends.
    error: Option<io::Error>,
}

impl<'a> PromptSaver<'a> {
    /// For a prompt about to join `history`, saved in `session` if any.
    pub fn new(session: Option<&'a mut Session>, history: &[ChatMessage]) -> Self {
        Self {
            session,
            saved: history.len(),
            error: None,
        }
    }

    /// Save what `event` says joined or replaced the history.
    pub fn on_event(&mut self, event: &Event<'_>) {
        let Some(session) = self.session.as_deref_mut() else {
            return;
        };
        let res = match event {
            Event::Messages(messages) => {
                self.saved += messages.len();
                session.writer.append_messages(messages)
            }
            Event::Compacted { messages, .. } => {
                self.saved = messages.len();
                session.context = 0;
                session.writer.append_compact(messages)
            }
            _ => return,
        };
        if let Err(err) = res {
            self.error.get_or_insert(err);
        }
    }

    /// Save what is left of the prompt once it ended, with the `history`
    /// it made, its token `usage`, and the tokens its last request sent
    /// (0 when unknown).
    pub fn finish(
        self,
        history: &[ChatMessage],
        usage: TokenUsage,
        context: u64,
    ) -> io::Result<()> {
        let Some(session) = self.session else {
            return Ok(());
        };
        let res = session.record_prompt(&history[self.saved..], usage, context);
        self.error.map_or(res, Err)
    }
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
    pub adapters: Vec<AdapterKind>,
    /// Shown next to the startup logo.
    pub banner: Vec<BannerLine>,
    /// Checked after every prompt, when sandboxed.
    pub git_guard: Option<GitGuard>,
    /// Colors of code blocks, instead of the default theme.
    pub code_theme: Option<CodeTheme>,
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
pub fn check_git_guard(guard: Option<&mut GitGuard>, changed_files: bool) -> Vec<String> {
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

/// The start of a session `id`: its creation date and time.
fn short_id(id: &str) -> &str {
    id.get(..15).unwrap_or(id)
}

/// Run the interactive inline TUI until the user quits (`Ctrl-D` or `/quit`).
pub async fn run(agent: Agent, status: Status, opts: RunOptions) -> Result<(), Error> {
    let (asker, mut questions) = mpsc::unbounded_channel();
    let mut agent = agent.with_asker(asker);
    let RunOptions {
        mut session,
        model: mut startup_model,
        mut history,
        picker_entries,
        adapters,
        banner,
        mut git_guard,
        code_theme,
    } = opts;
    let mut models = ModelCatalog {
        adapters,
        cached: None,
    };
    let mut term = InlineTerminal::enter(VIEWPORT_HEIGHT)?;
    let mut app = App::new(agent.model().to_owned(), status, Theme::from_env());
    app.set_width(term.width());
    app.set_thinking(agent.thinking());
    if let Some(theme) = code_theme {
        app.set_code_theme(theme);
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
    let mut events = EventReader::spawn();

    draw(&mut term, &mut app)?;

    loop {
        let Some(next) = events.next().await else {
            break;
        };
        let action = handle_event_batch(next?, &mut events, &mut app)?;
        // Only for the picker shown at startup, until anything else is done.
        let model = match action {
            Action::None | Action::Redraw => None,
            _ => startup_model.take(),
        };
        match action {
            Action::None | Action::Cancel | Action::Answer(_) => {}
            Action::Redraw => draw(&mut term, &mut app)?,
            Action::Quit => break,
            Action::Slash(cmd) => {
                if handle_slash(
                    cmd,
                    &mut term,
                    &mut agent,
                    &mut app,
                    &mut events,
                    &mut questions,
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
                resume_session(&id, model, &mut agent, &mut app, session.as_mut());
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
                let turn = run_turn(
                    &mut term,
                    &mut app,
                    &mut agent,
                    &mut events,
                    &mut questions,
                    session.as_mut(),
                    &prompt,
                )
                .await?;
                // Before the git check: a program left running could
                // recreate a repository right after it.
                agent.stop_background().await;
                let notices = check_git_guard(git_guard.as_mut(), turn.changed_files);
                if turn.quit {
                    // Once the screen is back.
                    drop(term);
                    for notice in &notices {
                        eprintln!("{}", printable(notice));
                    }
                    return Ok(());
                }
                if !notices.is_empty() {
                    notices.iter().for_each(|notice| app.push_info(notice));
                    draw(&mut term, &mut app)?;
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
#[allow(clippy::too_many_arguments)]
async fn handle_slash(
    cmd: SlashCommand,
    term: &mut InlineTerminal,
    agent: &mut Agent,
    app: &mut App,
    events: &mut EventReader,
    questions: &mut mpsc::UnboundedReceiver<Question>,
    session: Option<&mut Session>,
    models: &mut ModelCatalog,
) -> Result<bool, Error> {
    match cmd {
        SlashCommand::Quit => return Ok(true),
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

/// Switch to the session `id`, with its own model unless `model` is given.
fn resume_session(
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

#[derive(Default)]
struct DriveStats {
    usage: TokenUsage,
    changed_files: bool,
}

struct DriveResult {
    cancelled: bool,
    quit: bool,
    outcome: Option<Result<String, Error>>,
    stats: DriveStats,
}

struct TurnOutcome {
    quit: bool,
    changed_files: bool,
}

/// Drive an in-flight agent future (`prompt` or `compact`) while keeping the
/// UI responsive to keys, resize, questions, and streaming updates.
async fn drive_agent(
    term: &mut InlineTerminal,
    app: &mut App,
    events: &mut EventReader,
    questions: &mut mpsc::UnboundedReceiver<Question>,
    mut rx: mpsc::UnboundedReceiver<AgentUpdate>,
    fut: impl std::future::Future<Output = Result<String, Error>>,
) -> Result<DriveResult, Error> {
    let mut cancelled = false;
    let mut quit = false;
    let mut outcome: Option<Result<String, Error>> = None;
    let mut stats = DriveStats::default();
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
                            Action::None
                            | Action::Submit(_)
                            | Action::Slash(_)
                            | Action::ResumeSession(_)
                            | Action::SelectModel(_)
                            | Action::SetThinking(_) => {}
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
                    drain_updates(update, &mut rx, app, &mut stats);
                    draw(term, app)?;
                }
                // After `rx`, so the tool call shows before its question.
                Some(asked) = questions.recv() => {
                    app.ask(&asked.text, &asked.options);
                    question = Some(asked);
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
        drain_updates(first, &mut rx, app, &mut stats);
    }
    // A question sent just before a cancel has nobody waiting for it: drop
    // it so the next prompt does not show it.
    while questions.try_recv().is_ok() {}

    Ok(DriveResult {
        cancelled,
        quit,
        outcome,
        stats,
    })
}

/// Run one agent prompt while keeping the UI responsive to keys, resize and
/// streaming updates.
async fn run_turn(
    term: &mut InlineTerminal,
    app: &mut App,
    agent: &mut Agent,
    events: &mut EventReader,
    questions: &mut mpsc::UnboundedReceiver<Question>,
    session: Option<&mut Session>,
    prompt: &str,
) -> Result<TurnOutcome, Error> {
    let mut saver = PromptSaver::new(session, agent.history());
    let (tx, rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let res = drive_agent(
        term,
        app,
        events,
        questions,
        rx,
        agent.prompt(prompt, |event| {
            saver.on_event(&event);
            forward_event(&tx, event);
        }),
    )
    .await?;

    let changed_files = res.stats.changed_files;
    if let Err(err) = saver.finish(agent.history(), res.stats.usage, app.context()) {
        app.push_info(format!("[session save error: {err}]"));
    }

    if finish_activity(app, res) {
        return Ok(TurnOutcome {
            quit: true,
            changed_files,
        });
    }
    draw(term, app)?;
    Ok(TurnOutcome {
        quit: false,
        changed_files,
    })
}

/// Summarize the conversation with `/compact`, replacing history on success.
/// Returns `true` if the user requested to quit.
async fn run_compact(
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

    let (tx, rx) = mpsc::unbounded_channel::<AgentUpdate>();
    let res = drive_agent(
        term,
        app,
        events,
        questions,
        rx,
        agent.compact(focus, |event| forward_event(&tx, event)),
    )
    .await?;

    let compacted = !res.cancelled && !res.quit && matches!(res.outcome, Some(Ok(_)));
    let usage = res.stats.usage;
    if finish_activity(app, res) {
        return Ok(true);
    }
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
    stats: &mut DriveStats,
) {
    let mut pending = first;
    while let Ok(next) = rx.try_recv() {
        match (&mut pending, next) {
            (AgentUpdate::TextDelta(acc), AgentUpdate::TextDelta(more))
            | (AgentUpdate::ReasoningDelta(acc), AgentUpdate::ReasoningDelta(more)) => {
                acc.push_str(&more);
            }
            (_, next) => {
                apply_and_track(pending, app, stats);
                pending = next;
            }
        }
    }
    apply_and_track(pending, app, stats);
}

fn apply_and_track(update: AgentUpdate, app: &mut App, stats: &mut DriveStats) {
    match &update {
        AgentUpdate::TurnEnd(usage) => stats.usage += *usage,
        AgentUpdate::ToolCall { name, .. } if !is_read_only(name) => stats.changed_files = true,
        _ => {}
    }
    app.apply_update(update);
}

fn draw(term: &mut InlineTerminal, app: &mut App) -> Result<(), Error> {
    term.draw_app(app)?;
    Ok(())
}
