// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The session a conversation is saved to.

use std::io;

use genai::chat::ChatMessage;
use harness_core::{
    Error, Event, SessionData, SessionStore, SessionSummary, SessionWriter, Thinking, TokenUsage,
    ToolDetails,
};

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

    /// Persist that the conversation went back to `messages`, to send a
    /// prompt again.
    pub fn record_rewind(&mut self, messages: &[ChatMessage]) -> io::Result<()> {
        self.writer.append_compact(messages)
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

    /// Persist a thinking effort change.
    pub fn record_thinking(&mut self, thinking: Thinking) -> io::Result<()> {
        self.writer.append_thinking(thinking)
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

    /// Go on in a new session holding a copy of this conversation, its
    /// `history`, leaving the file of this one as it is.
    pub fn fork(
        &mut self,
        model: &str,
        thinking: Thinking,
        history: &[ChatMessage],
    ) -> io::Result<()> {
        self.writer =
            self.store
                .fork(self.writer.id(), model, history, self.usage, self.context)?;
        self.writer.append_thinking(thinking)
    }

    /// Go on with the loaded session `data` using `model` and `thinking`,
    /// saving them where they differ from its own, or with `fork` in a copy
    /// of it.
    pub fn resume(
        &mut self,
        data: &SessionData,
        model: &str,
        thinking: Thinking,
        fork: bool,
    ) -> io::Result<()> {
        if fork {
            return self.fork(model, thinking, &data.messages);
        }
        if model != data.meta.model {
            self.writer.append_model(model)?;
        }
        if data.meta.thinking != Some(thinking) {
            self.writer.append_thinking(thinking)?;
        }
        Ok(())
    }

    /// Start a fresh lazy session file (e.g. after `/new`), leaving the
    /// previous session file intact on disk.
    pub fn rotate(&mut self, model: &str, thinking: Thinking) {
        self.writer = self.store.create(model).with_thinking(thinking);
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
            Event::ToolResult(resp, Some(ToolDetails::Edit(diff))) => {
                session.writer.append_diff(&resp.call_id, diff)
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

/// The start of a session `id`: its creation date and time.
pub(crate) fn short_id(id: &str) -> &str {
    id.get(..15).unwrap_or(id)
}
