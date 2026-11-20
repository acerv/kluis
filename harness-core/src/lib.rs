// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Agent core for LLM harnesses.
//!
//! An [`Agent`] talks to a model and runs the tool calls it makes,
//! reporting its progress as [`Event`]s; [`tools`] holds the tools it
//! offers and their dispatch.

mod agent;
mod ask;
mod auth;
mod error;
mod event;
mod fetch;
mod models;
pub mod oauth;
mod private_file;
mod prompt;
mod retry;
mod session;
pub mod tools;

pub use agent::{Agent, Error, Thinking};
pub use auth::{AuthConfig, CredentialSource, ProviderAuth};
pub use event::Event;
pub use models::{
    discover_models, discover_models_with_timeout, is_chat_model, provider_name, ModelInfo,
    ModelList,
};
pub use prompt::{find_agents_files, system_prompt, AGENTS_FILE, MAX_AGENTS_BYTES};
pub use retry::RetryPolicy;
pub use session::{
    SessionData, SessionEntry, SessionMeta, SessionStore, SessionSummary, SessionWriter,
    TokenUsage, MAX_SESSION_BYTES,
};
pub use tools::ToolDetails;
