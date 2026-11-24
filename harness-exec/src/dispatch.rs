// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tool definitions and dispatch.

use std::path::PathBuf;

use genai::chat::{Tool, ToolCall, ToolResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    BashContext, BashTool, Diff, EditTool, FindTool, GrepTool, Image, LsTool, ReadTool, Todo,
    TodoTool, WriteTool, BASH_TOOL, EDIT_TOOL, FIND_TOOL, GREP_TOOL, LS_TOOL, READ_TOOL, TODO_TOOL,
    WRITE_TOOL,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Extra tool output, not part of the text sent back to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolDetails {
    /// For the UI.
    Edit(Diff),
    /// For the UI.
    Todo(Vec<Todo>),
    /// For the model, which gets it next to the text.
    Image(Image),
}

/// How tools run locally.
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
        BashTool::definition(false),
        TodoTool::definition(),
    ]
}

/// Whether `name` is a read-only tool that can safely run in parallel with
/// other read-only tools.
pub fn is_read_only(name: &str) -> bool {
    matches!(name, READ_TOOL | LS_TOOL | FIND_TOOL | GREP_TOOL)
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
    execute_streaming(call, ctx, |_| {}).await
}

/// Like [`execute`], streaming cleaned live output chunks (e.g. from `bash`)
/// via `on_output` while the tool runs.
pub async fn execute_streaming(
    call: &ToolCall,
    ctx: &ToolContext,
    on_output: impl FnMut(&str),
) -> (ToolResponse, Option<ToolDetails>) {
    let (content, details) = dispatch_raw(&call.fn_name, &call.fn_arguments, ctx, on_output).await;
    (ToolResponse::from_tool_call(call, content), details)
}

/// Dispatch one tool call by name and JSON arguments, returning serialized
/// output (or `{"error": "..."}`) plus optional UI details.
pub async fn dispatch_raw(
    name: &str,
    args: &Value,
    ctx: &ToolContext,
    on_output: impl FnMut(&str),
) -> (String, Option<ToolDetails>) {
    match dispatch(name, args, ctx, on_output).await {
        Ok(pair) => pair,
        Err(err) => (json!({ "error": err.to_string() }).to_string(), None),
    }
}

async fn dispatch(
    name: &str,
    args: &Value,
    ctx: &ToolContext,
    on_output: impl FnMut(&str),
) -> Result<(String, Option<ToolDetails>), Error> {
    let cwd = &ctx.cwd;
    match name {
        READ_TOOL => {
            let mut out = ReadTool::deserialize(args)?.run_in(cwd).await?;
            let image = out.image.take().map(ToolDetails::Image);
            Ok((serde_json::to_string(&out)?, image))
        }
        WRITE_TOOL => plain(&WriteTool::deserialize(args)?.run_in(cwd).await?),
        EDIT_TOOL => {
            let out = EditTool::deserialize(args)?.run_in(cwd).await?;
            Ok((
                serde_json::to_string(&out)?,
                Some(ToolDetails::Edit(out.diff)),
            ))
        }
        LS_TOOL => plain(&LsTool::deserialize(args)?.run_in(cwd).await?),
        FIND_TOOL => plain(&FindTool::deserialize(args)?.run_in(cwd).await?),
        GREP_TOOL => plain(&GrepTool::deserialize(args)?.run_in(cwd).await?),
        BASH_TOOL => {
            let tool = BashTool::deserialize(args)?;
            if tool.background {
                plain(&tool.start_in(cwd, &ctx.bash).await?)
            } else {
                plain(&tool.run_in_streaming(cwd, &ctx.bash, on_output).await?)
            }
        }
        TODO_TOOL => {
            let out = TodoTool::deserialize(args)?.run()?;
            Ok((
                serde_json::to_string(&out)?,
                Some(ToolDetails::Todo(out.todos)),
            ))
        }
        other => Err(format!("unknown tool {other}").into()),
    }
}

fn plain(output: &impl Serialize) -> Result<(String, Option<ToolDetails>), Error> {
    Ok((serde_json::to_string(output)?, None))
}
