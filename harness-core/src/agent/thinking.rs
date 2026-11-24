// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The thinking effort asked of the model.

use std::fmt;
use std::str::FromStr;

use genai::chat::ReasoningEffort;
use serde::{Deserialize, Serialize};

/// Thinking / reasoning effort sent to models that support it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    /// Leave the provider's default reasoning behavior untouched.
    #[default]
    Default,
    /// Disable reasoning where the provider supports turning it off.
    Off,
    Low,
    Medium,
    High,
    /// Extra-high effort, for models that take it.
    XHigh,
    /// Maximum effort, for models that take it.
    Max,
}

impl Thinking {
    /// Every level, in increasing effort after `Default`.
    pub const ALL: [Self; 7] = [
        Self::Default,
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// Lowercase effort name (`"default"`, `"off"`, `"low"`, ... `"max"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub(super) fn reasoning_effort(self) -> Option<ReasoningEffort> {
        match self {
            Self::Default => None,
            Self::Off => Some(ReasoningEffort::Zero),
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
            Self::XHigh => Some(ReasoningEffort::XHigh),
            Self::Max => Some(ReasoningEffort::Max),
        }
    }
}

impl fmt::Display for Thinking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Thinking {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(trimmed))
            .ok_or_else(|| {
                format!(
                    "invalid thinking effort '{s}' (expected default, off, low, medium, high, xhigh, or max)"
                )
            })
    }
}
