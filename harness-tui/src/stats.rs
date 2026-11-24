// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! What the events of a prompt add up to.

use harness_core::tools::is_read_only;
use harness_core::{Event, TokenUsage};

/// What a prompt did so far, from its [`Event`]s.
#[derive(Debug, Default)]
pub struct PromptStats {
    /// The tokens it used, the ones of its subagents too.
    pub usage: TokenUsage,
    /// The tokens its last request sent, or 0 when unknown, as after a
    /// compaction.
    pub context: u64,
    /// Whether a tool that may change files was called.
    pub changed_files: bool,
    /// The last text of the model, a summary of the conversation left out.
    pub text: String,
    compacting: bool,
}

impl PromptStats {
    /// Whether the model is writing a summary of the conversation, not an
    /// answer.
    pub fn compacting(&self) -> bool {
        self.compacting
    }

    /// Count `event`.
    pub fn on_event(&mut self, event: &Event<'_>) {
        match event {
            Event::TurnEnd(usage) => {
                if let Some(usage) = usage {
                    let turn = TokenUsage::from_genai(usage);
                    self.usage += turn;
                    self.context = turn.prompt();
                }
                self.compacting = false;
            }
            Event::TaskUsage(usage) => self.usage += TokenUsage::from_genai(usage),
            Event::Compacting { .. } => self.compacting = true,
            Event::CompactFailed(_) => self.compacting = false,
            Event::Compacted { .. } => {
                self.compacting = false;
                self.context = 0;
            }
            Event::ToolCall(call) if !is_read_only(&call.fn_name) => self.changed_files = true,
            Event::Text(text) if !self.compacting => self.text = (*text).to_owned(),
            _ => {}
        }
    }
}
