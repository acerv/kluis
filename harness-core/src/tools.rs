// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The tools offered to the model and the dispatch of its calls.

use std::path::PathBuf;

use genai::chat::{Tool, ToolCall, ToolResponse};
use harness_tools::{BashTool, EditTool, FindTool, LsTool, ReadTool, WriteTool};
use serde::Deserialize;
use serde_json::json;

pub use harness_tools::{BashContext, PreExec};

type Error = Box<dyn std::error::Error + Send + Sync>;

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
    let content = match dispatch(call, ctx).await {
        Ok(output) => output,
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    };
    ToolResponse::from_tool_call(call, content)
}

/// Run a call and return its output serialized as JSON.
async fn dispatch(call: &ToolCall, ctx: &ToolContext) -> Result<String, Error> {
    let args = &call.fn_arguments;
    let cwd = &ctx.cwd;
    Ok(match call.fn_name.as_str() {
        "read" => serde_json::to_string(&ReadTool::deserialize(args)?.run_in(cwd).await?)?,
        "write" => serde_json::to_string(&WriteTool::deserialize(args)?.run_in(cwd).await?)?,
        "edit" => serde_json::to_string(&EditTool::deserialize(args)?.run_in(cwd).await?)?,
        "ls" => serde_json::to_string(&LsTool::deserialize(args)?.run_in(cwd).await?)?,
        "find" => serde_json::to_string(&FindTool::deserialize(args)?.run_in(cwd).await?)?,
        "bash" => {
            let tool = BashTool::deserialize(args)?;
            serde_json::to_string(&tool.run_in(cwd, &ctx.bash).await?)?
        }
        other => return Err(format!("unknown tool {other}").into()),
    })
}
