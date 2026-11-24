// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tools and async sandboxed tool executor for LLM harnesses.
//!
//! Each tool is a type holding the input of one call. Its `definition()`
//! describes the tool to the model (a `genai` tool). [`Executor`] runs tool
//! calls either directly in-process ([`Executor::Local`]) or inside a
//! persistent `hakoniwa` container ([`Executor::Sandbox`]) over an async
//! NDJSON stdio protocol supporting parallel calls and per-call cancellation.
//!
//! Only Linux is supported.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-exec only supports Linux");

mod bash;
mod diff;
mod dispatch;
mod edit;
mod error;
mod find;
mod grep;
mod ls;
mod path;
mod protocol;
mod read;
mod sandbox;
mod todo;
mod walk;
mod worker;
mod write;

use genai::chat::{ToolCall, ToolResponse};

pub use bash::{
    BashContext, BashOutput, BashTool, JobOutput, Jobs, OutputFiles, PreExec, DEFAULT_BASH_TIMEOUT,
    MAX_BASH_LINES, MAX_BASH_LOG_BYTES, MAX_BASH_OUTPUT,
};
pub use diff::{
    count_lines, diff_lines, Diff, DiffHunk, DiffLine, DIFF_CONTEXT_LINES, MAX_DIFF_BYTES,
};
pub use dispatch::{
    definitions, dispatch_raw, execute, execute_streaming, is_read_only, run, run_with,
    ToolContext, ToolDetails,
};
pub use edit::{Edit, EditOutput, EditTool, MAX_EDIT_BYTES};
pub use error::ToolError;
pub use find::{FindOutput, FindTool, DEFAULT_FIND_LIMIT, MAX_FIND_BYTES};
pub use grep::{GrepOutput, GrepTool, DEFAULT_GREP_LIMIT, MAX_GREP_BYTES, MAX_GREP_LINE_CHARS};
pub use ls::{LsOutput, LsTool, DEFAULT_LS_LIMIT, MAX_LS_BYTES};
pub use protocol::{Request, Response, MAX_FRAME_BYTES};
pub use read::{Image, ReadOutput, ReadTool, DEFAULT_READ_LIMIT, MAX_IMAGE_BYTES, MAX_READ_BYTES};
pub use sandbox::{ExecError, SandboxConfig, SandboxExecutor};
pub use todo::{
    Todo, TodoOutput, TodoStatus, TodoTool, MAX_TODO_ITEMS, MAX_TODO_ITEM_BYTES, TODO_TOOL,
};
pub use worker::{run_worker, run_worker_io};
pub use write::{WriteOutput, WriteTool};

/// Executes tool calls either directly in-process or inside a sandboxed worker
/// container.
#[derive(Debug)]
pub enum Executor {
    Local(ToolContext),
    Sandbox(Box<SandboxExecutor>),
}

impl Default for Executor {
    fn default() -> Self {
        Self::Local(ToolContext::default())
    }
}

impl Executor {
    /// Spawn a sandboxed [`Executor`].
    pub fn sandbox(config: SandboxConfig) -> Result<Self, ExecError> {
        Ok(Self::Sandbox(Box::new(SandboxExecutor::new(config)?)))
    }

    /// Execute one tool call, streaming cleaned live output chunks via
    /// `on_output`. Safe to call concurrently from multiple futures.
    pub async fn execute(
        &self,
        call: &ToolCall,
        on_output: impl FnMut(&str),
    ) -> (ToolResponse, Option<ToolDetails>) {
        match self {
            Self::Local(ctx) => execute_streaming(call, ctx, on_output).await,
            Self::Sandbox(sandbox) => sandbox.execute(call, on_output).await,
        }
    }

    /// Stop what commands left running in the background, in the sandbox
    /// only: elsewhere they may be the user's own processes. With
    /// `keep_jobs`, the commands the `bash` tool started in the background
    /// keep running; they are stopped anywhere otherwise.
    ///
    /// Returns whether such commands were running, as they may have
    /// changed files since, or when this is not known.
    pub async fn stop_background(&self, keep_jobs: bool) -> bool {
        match self {
            Self::Local(ctx) => ctx.bash.jobs.stop_unless(keep_jobs),
            Self::Sandbox(sandbox) => sandbox.stop_background(keep_jobs).await,
        }
    }
}
