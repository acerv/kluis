//! Minimal tools for LLM harnesses.
//!
//! The host deserializes a [`ToolCall`] from model output, runs [`execute`],
//! and serializes the resulting [`ToolOutput`] or [`ToolError`] back.

mod call;
mod error;
mod path;
mod tools;

pub use call::{execute, ToolCall, ToolOutput};
pub use error::ToolError;
pub use tools::read::{read, ReadOutput, ReadParams, DEFAULT_READ_LIMIT};
pub use tools::write::{write, WriteOutput, WriteParams};
