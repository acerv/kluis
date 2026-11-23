// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The tools offered to the model and the dispatch of its calls.

pub use harness_exec::{
    definitions, execute, execute_streaming, is_read_only, run, run_with, run_worker, BashContext,
    Diff, DiffHunk, DiffLine, ExecError, Executor, OutputFiles, PreExec, SandboxConfig,
    SandboxExecutor, Todo, TodoOutput, TodoStatus, TodoTool, ToolContext, ToolDetails, ToolError,
    TODO_TOOL,
};

use genai::chat::{ToolCall, ToolResponse};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use crate::ask::{AskOutput, AskTool, Asker, Question, ASK_TOOL, MAX_ASK_OPTIONS};
pub use crate::fetch::{
    FetchOutput, FetchTool, FETCH_TIMEOUT, FETCH_TOOL, MAX_FETCH_BODY_BYTES, MAX_FETCH_BYTES,
};

/// What the tools the agent runs itself, not the executor, need.
#[derive(Debug, Default)]
pub(crate) struct HostTools {
    pub asker: Option<Asker>,
    pub fetcher: Option<Client>,
}

impl HostTools {
    /// Run `call` when it is one of these tools.
    pub async fn run(&self, call: &ToolCall) -> Option<ToolResponse> {
        let args = &call.fn_arguments;
        let result = match call.fn_name.as_str() {
            ASK_TOOL => {
                output(async { parse::<AskTool>(args)?.run(self.asker.as_ref()).await }.await)
            }
            FETCH_TOOL => {
                output(async { parse::<FetchTool>(args)?.run(self.fetcher.as_ref()).await }.await)
            }
            _ => return None,
        };
        Some(match result {
            Ok(content) => ToolResponse::from_tool_call(call, content),
            Err(err) => error_response(call, &err.message),
        })
    }
}

/// A tool response carrying `message` as `{"error": "..."}`.
pub(crate) fn error_response(call: &ToolCall, message: &str) -> ToolResponse {
    ToolResponse::from_tool_call(call, json!({ "error": message }).to_string())
}

fn parse<'a, T: Deserialize<'a>>(args: &'a serde_json::Value) -> Result<T, ToolError> {
    T::deserialize(args).map_err(|e| ToolError::new(e.to_string()))
}

fn output(result: Result<impl Serialize, ToolError>) -> Result<String, ToolError> {
    let out = result?;
    serde_json::to_string(&out).map_err(|e| ToolError::new(e.to_string()))
}
