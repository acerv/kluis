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
mod commands;
mod error;
mod event;
mod fetch;
mod front_matter;
mod mention;
mod models;
pub mod oauth;
mod private_file;
mod prompt;
mod retry;
mod search;
mod session;
mod skills;
pub mod tools;

pub use agent::{Agent, Error, Steering, Thinking, DEFAULT_AUTO_COMPACT_TOKENS, DEFAULT_MAX_TURNS};
pub use auth::{AuthConfig, CredentialSource, ProviderAuth};
pub use commands::{
    expand_command, find_commands, Command, Commands, ARGUMENTS, MAX_COMMANDS, MAX_COMMAND_BYTES,
};
pub use event::Event;
pub use models::{
    discover_models, discover_models_with_timeout, is_chat_model, provider_name, thinking_levels,
    ModelInfo, ModelList,
};
pub use prompt::{find_agents_files, system_prompt, AGENTS_FILE, MAX_AGENTS_BYTES};
pub use retry::RetryPolicy;
pub use session::{
    SessionData, SessionEntry, SessionMeta, SessionStore, SessionSummary, SessionWriter,
    TokenUsage, MAX_SESSION_BYTES,
};
pub use skills::{find_skills, Skill, Skills, MAX_SKILLS, SKILL_FILE};
pub use tools::ToolDetails;
