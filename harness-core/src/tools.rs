// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The tools offered to the model and the dispatch of its calls.

use genai::chat::{Tool, ToolCall, ToolResponse};
use harness_tools::{BashTool, EditTool, ReadTool, WriteTool};
use serde::Deserialize;
use serde_json::json;

pub use harness_tools::{BashContext, PreExec};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// How the host wants tools to run.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    pub bash: BashContext,
}

/// Definitions of the tools offered to the model.
pub fn definitions() -> Vec<Tool> {
    vec![
        ReadTool::definition(),
        WriteTool::definition(),
        EditTool::definition(),
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
    Ok(match call.fn_name.as_str() {
        "read" => serde_json::to_string(&ReadTool::deserialize(args)?.run().await?)?,
        "write" => serde_json::to_string(&WriteTool::deserialize(args)?.run().await?)?,
        "edit" => serde_json::to_string(&EditTool::deserialize(args)?.run().await?)?,
        "bash" => {
            let tool = BashTool::deserialize(args)?;
            serde_json::to_string(&tool.run_with(&ctx.bash).await?)?
        }
        other => return Err(format!("unknown tool {other}").into()),
    })
}
