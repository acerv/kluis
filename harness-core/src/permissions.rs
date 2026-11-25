// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Which tools run freely, which wait for the user to approve each call,
//! and which the model does not get at all.
//!
//! This is a convenience for the user, not a boundary: the sandbox is.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use genai::chat::ToolCall;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::ask::{Asker, Question};
use crate::tools;

/// Every tool an agent may offer.
const TOOLS: &[&str] = &[
    tools::READ_TOOL,
    tools::WRITE_TOOL,
    tools::EDIT_TOOL,
    tools::LS_TOOL,
    tools::FIND_TOOL,
    tools::GREP_TOOL,
    tools::BASH_TOOL,
    tools::TODO_TOOL,
    tools::TASK_TOOL,
    tools::ASK_TOOL,
    tools::FETCH_TOOL,
    tools::SEARCH_TOOL,
    tools::SKILL_TOOL,
];

/// Answers offered to the user for a call waiting for approval.
const YES: &str = "Yes";
const ALWAYS: &str = "Always in this session";
const NO: &str = "No";

/// Longest value of an argument shown to the user, in characters.
const MAX_SHOWN_CHARS: usize = 2000;

const REFUSED: &str = "the user refused this call";

/// What may happen to the calls of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// They run.
    Allow,
    /// Each waits for the user to approve it.
    Ask,
    /// The model does not get the tool.
    Deny,
}

/// The permission of each tool; a tool without one is allowed.
#[derive(Debug, Clone, Default)]
pub struct Permissions {
    pub(crate) rules: HashMap<String, Permission>,
    /// Whom to ask for an approval.
    pub(crate) asker: Option<Asker>,
    /// The tools the user allowed until the end of the session. Held
    /// while the user is asked: one question at a time.
    always: Arc<Mutex<HashSet<String>>>,
}

impl Permissions {
    /// Give `tool` the permission `permission`, replacing the one it had.
    /// The tool must exist, and asking the user needs no approval.
    pub fn set(&mut self, tool: &str, permission: Permission) -> Result<(), String> {
        if !TOOLS.contains(&tool) {
            return Err(format!("unknown tool `{tool}`"));
        }
        if tool == tools::ASK_TOOL && permission == Permission::Ask {
            return Err(format!("`{tool}` cannot wait for an approval"));
        }
        self.rules.insert(tool.to_owned(), permission);
        Ok(())
    }

    /// Whether every tool is allowed.
    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn of(&self, tool: &str) -> Permission {
        self.rules.get(tool).copied().unwrap_or(Permission::Allow)
    }

    /// Whether the model gets `tool`: it is not denied, and when its calls
    /// wait for an approval there is someone to ask.
    pub(crate) fn offers(&self, tool: &str) -> bool {
        match self.of(tool) {
            Permission::Allow => true,
            Permission::Ask => self.asker.is_some(),
            Permission::Deny => false,
        }
    }

    /// Why `call` must not run, if so, asking the user when its tool
    /// says to. `tag` names the subagent making the call.
    pub(crate) async fn refusal(&self, call: &ToolCall, tag: &str) -> Option<String> {
        let tool = call.fn_name.as_str();
        let asker = match (self.of(tool), &self.asker) {
            (Permission::Allow, _) => return None,
            (Permission::Ask, Some(asker)) => asker,
            _ => return Some(format!("the user does not allow `{tool}`")),
        };
        let mut always = self.always.lock().await;
        if always.contains(tool) {
            return None;
        }
        let (question, answer) = Question::new(
            format!("{tag}Allow {tool}?{}", shown(&call.fn_arguments)),
            [YES, ALWAYS, NO].map(str::to_owned).to_vec(),
        );
        if asker.send(question).is_err() {
            return Some(REFUSED.to_owned());
        }
        match answer.await.as_deref() {
            Ok(YES) => None,
            Ok(ALWAYS) => {
                always.insert(tool.to_owned());
                None
            }
            Ok(NO) | Err(_) => Some(REFUSED.to_owned()),
            // What the user typed instead says why.
            Ok(reason) => Some(format!("{REFUSED}: {reason}")),
        }
    }
}

/// The arguments `args` of a call as lines for the user, one argument
/// each, long values cut.
fn shown(args: &Value) -> String {
    let mut text = String::new();
    for (name, value) in args.as_object().into_iter().flatten() {
        let value = match value {
            Value::String(value) => value.clone(),
            value => value.to_string(),
        };
        text.push_str(&format!("\n{name}: "));
        text.extend(value.chars().take(MAX_SHOWN_CHARS));
        if value.chars().nth(MAX_SHOWN_CHARS).is_some() {
            text.push('…');
        }
    }
    text
}
