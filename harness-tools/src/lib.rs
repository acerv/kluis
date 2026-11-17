// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Minimal tools for LLM harnesses.
//!
//! Each tool is a type holding the input of one call. Its `definition()`
//! describes the tool to the model (a `genai` tool). The host
//! deserializes the model's tool-call arguments into the type, calls
//! `run()`, and sends back the output or [`ToolError`].

mod error;
mod path;
mod tools;

pub use error::ToolError;
pub use tools::read::{ReadOutput, ReadTool, DEFAULT_READ_LIMIT};
pub use tools::write::{WriteOutput, WriteTool};
