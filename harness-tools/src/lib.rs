// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Minimal tools for LLM harnesses.
//!
//! Each tool is a type holding the input of one call. Its `definition()`
//! describes the tool to the model (a `genai` tool). The host
//! deserializes the model's tool-call arguments into the type, calls
//! `run()`, and sends back the output or [`ToolError`].
//!
//! Only Linux is supported.

#[cfg(not(target_os = "linux"))]
compile_error!("harness-tools only supports Linux");

mod bash;
mod edit;
mod error;
mod path;
mod read;
mod write;

pub use bash::{
    BashContext, BashOutput, BashTool, PreExec, DEFAULT_BASH_TIMEOUT, MAX_BASH_LINES,
    MAX_BASH_OUTPUT,
};
pub use edit::{Edit, EditOutput, EditTool};
pub use error::ToolError;
pub use read::{ReadOutput, ReadTool, DEFAULT_READ_LIMIT, MAX_READ_BYTES};
pub use write::{WriteOutput, WriteTool};
