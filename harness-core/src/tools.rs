// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The tools offered to the model and the dispatch of its calls.

use genai::chat::{Tool, ToolCall, ToolResponse};
use harness_tools::{EditTool, ReadTool, WriteTool};
use serde::Deserialize;
use serde_json::json;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Definitions of the tools offered to the model.
pub fn definitions() -> Vec<Tool> {
    vec![
        ReadTool::definition(),
        WriteTool::definition(),
        EditTool::definition(),
    ]
}

/// Run a model tool call and build the response to send back.
///
/// The content is the tool output as JSON, or `{"error": "..."}` when the
/// call is invalid or the tool fails, so the model can recover.
pub async fn run(call: &ToolCall) -> ToolResponse {
    let content = match dispatch(call).await {
        Ok(output) => output,
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    };
    ToolResponse::from_tool_call(call, content)
}

/// Run a call and return its output serialized as JSON.
async fn dispatch(call: &ToolCall) -> Result<String, Error> {
    let args = &call.fn_arguments;
    Ok(match call.fn_name.as_str() {
        "read" => serde_json::to_string(&ReadTool::deserialize(args)?.run().await?)?,
        "write" => serde_json::to_string(&WriteTool::deserialize(args)?.run().await?)?,
        "edit" => serde_json::to_string(&EditTool::deserialize(args)?.run().await?)?,
        other => return Err(format!("unknown tool {other}").into()),
    })
}
