// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Agent core for LLM harnesses.
//!
//! An [`Agent`] talks to a model and runs the tool calls it makes,
//! reporting its progress as [`Event`]s; [`tools`] holds the tools it
//! offers and their dispatch.

mod agent;
mod event;
pub mod tools;

pub use agent::{Agent, Error};
pub use event::Event;
pub use tools::ToolDetails;
