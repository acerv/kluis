// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The tools offered to the model and the dispatch of its calls.

use genai::chat::{Tool, ToolCall, ToolResponse};
use harness_tools::{ReadTool, WriteTool};
use serde::Deserialize;
use serde_json::{json, Value};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Definitions of the tools offered to the model.
pub fn definitions() -> Vec<Tool> {
    vec![ReadTool::definition(), WriteTool::definition()]
}

/// Run a model tool call and build the response to send back.
///
/// The content is the tool output as JSON, or `{"error": "..."}` when the
/// call is invalid or the tool fails, so the model can recover.
pub async fn run(call: &ToolCall) -> ToolResponse {
    let content = match dispatch(call).await {
        Ok(output) => output.to_string(),
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    };
    ToolResponse::from_tool_call(call, content)
}

async fn dispatch(call: &ToolCall) -> Result<Value, Error> {
    let args = &call.fn_arguments;
    Ok(match call.fn_name.as_str() {
        "read" => json!(ReadTool::deserialize(args)?.run().await?),
        "write" => json!(WriteTool::deserialize(args)?.run().await?),
        other => return Err(format!("unknown tool {other}").into()),
    })
}
