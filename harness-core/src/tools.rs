// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The tools offered to the model and the dispatch of its calls.

use std::path::PathBuf;

use genai::chat::{Tool, ToolCall, ToolResponse};
use harness_tools::{BashTool, EditTool, FindTool, GrepTool, LsTool, ReadTool, WriteTool};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use harness_tools::{BashContext, Diff, DiffHunk, DiffLine, PreExec};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Extra tool output meant for the UI, not sent back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDetails {
    Edit(Diff),
}

/// How the host wants tools to run.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Folder relative paths start from, and commands run in. Defaults to
    /// `.`, the current directory of the process.
    pub cwd: PathBuf,
    pub bash: BashContext,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            bash: BashContext::default(),
        }
    }
}

/// Definitions of the tools offered to the model.
pub fn definitions() -> Vec<Tool> {
    vec![
        ReadTool::definition(),
        WriteTool::definition(),
        EditTool::definition(),
        LsTool::definition(),
        FindTool::definition(),
        GrepTool::definition(),
        BashTool::definition(),
    ]
}

/// Run a model tool call and build the response to send back.
///
/// The content is the tool output as JSON, or `{"error": "..."}` when the
/// call is invalid or the tool fails, so the model can recover.
pub async fn run(call: &ToolCall) -> ToolResponse {
    run_with(call, &ToolContext::default()).await
}

/// Like [`run`], with the host's [`ToolContext`].
pub async fn run_with(call: &ToolCall, ctx: &ToolContext) -> ToolResponse {
    execute(call, ctx).await.0
}

/// Like [`run_with`], also returning any UI-only [`ToolDetails`].
pub async fn execute(call: &ToolCall, ctx: &ToolContext) -> (ToolResponse, Option<ToolDetails>) {
    let (content, details) = match dispatch(call, ctx).await {
        Ok(pair) => pair,
        Err(err) => (json!({ "error": err.to_string() }).to_string(), None),
    };
    (ToolResponse::from_tool_call(call, content), details)
}

/// Run a call and return its JSON output for the model plus any UI details.
async fn dispatch(
    call: &ToolCall,
    ctx: &ToolContext,
) -> Result<(String, Option<ToolDetails>), Error> {
    let args = &call.fn_arguments;
    let cwd = &ctx.cwd;
    match call.fn_name.as_str() {
        "read" => plain(&ReadTool::deserialize(args)?.run_in(cwd).await?),
        "write" => plain(&WriteTool::deserialize(args)?.run_in(cwd).await?),
        "edit" => {
            let out = EditTool::deserialize(args)?.run_in(cwd).await?;
            Ok((
                serde_json::to_string(&out)?,
                Some(ToolDetails::Edit(out.diff)),
            ))
        }
        "ls" => plain(&LsTool::deserialize(args)?.run_in(cwd).await?),
        "find" => plain(&FindTool::deserialize(args)?.run_in(cwd).await?),
        "grep" => plain(&GrepTool::deserialize(args)?.run_in(cwd).await?),
        "bash" => plain(&BashTool::deserialize(args)?.run_in(cwd, &ctx.bash).await?),
        other => Err(format!("unknown tool {other}").into()),
    }
}

fn plain(output: &impl Serialize) -> Result<(String, Option<ToolDetails>), Error> {
    Ok((serde_json::to_string(output)?, None))
}
