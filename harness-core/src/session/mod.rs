// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Append-only JSONL session persistence scoped per workspace directory.
//!
//! Only Linux is supported.

use std::ops::AddAssign;
use std::path::PathBuf;

use genai::chat::{ChatMessage, Usage};
use serde::{Deserialize, Serialize};

mod load;
mod store;
mod writer;

pub use store::SessionStore;
pub use writer::SessionWriter;

/// Current JSONL session format version.
const FORMAT_VERSION: u32 = 1;

/// Maximum bytes read when loading a single session file.
pub const MAX_SESSION_BYTES: usize = 16 * 1024 * 1024;

/// One line in a `.jsonl` session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Header {
        version: u32,
        id: String,
        cwd: PathBuf,
        model: String,
        created_at: u64,
    },
    Model {
        model: String,
    },
    Message {
        message: ChatMessage,
    },
    Compact {
        messages: Vec<ChatMessage>,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        cached_tokens: u64,
        /// Tokens the last request of the prompt sent: 0 when unknown.
        #[serde(default)]
        context_tokens: u64,
    },
}

/// Token counts for a turn or session.
///
/// `input` holds non-cached prompt tokens (`prompt_tokens - cached`), `output`
/// holds completion tokens (including reasoning), and `cached` holds prompt
/// tokens served from cache.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
}

impl TokenUsage {
    /// Extract normalized token counts from a provider [`Usage`] record.
    pub fn from_genai(usage: &Usage) -> Self {
        let prompt = non_negative(usage.prompt_tokens);
        let output = non_negative(usage.completion_tokens);
        let cached = non_negative(
            usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
        );
        Self {
            input: prompt.saturating_sub(cached),
            output,
            cached,
        }
    }

    pub fn is_empty(self) -> bool {
        self.input == 0 && self.output == 0 && self.cached == 0
    }

    /// Tokens the request sent, cached or not: for one request, the size of
    /// the conversation.
    pub fn prompt(self) -> u64 {
        self.input.saturating_add(self.cached)
    }
}

fn non_negative(value: Option<i32>) -> u64 {
    value.and_then(|n| u64::try_from(n).ok()).unwrap_or(0)
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cached = self.cached.saturating_add(rhs.cached);
    }
}

/// Summary metadata of a persisted session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub model: String,
    pub created_at: u64,
}

/// Lightweight entry shown in session lists and pickers without keeping full
/// conversation payloads in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub model: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: usize,
    pub first_prompt: Option<String>,
}

/// Loaded session ready to resume in an [`crate::Agent`].
#[derive(Debug, Clone)]
pub struct SessionData {
    pub meta: SessionMeta,
    pub messages: Vec<ChatMessage>,
    pub usage: TokenUsage,
    /// Tokens the last request sent, or 0 when unknown.
    pub context: u64,
    /// If the file had a truncated trailing line or lacked a final newline,
    /// the byte length of the valid prefix to repair before appending.
    pub(super) repair_len: Option<u64>,
}
