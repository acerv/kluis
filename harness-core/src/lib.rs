// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Agent core for LLM harnesses.
//!
//! An [`Agent`] talks to a model and runs the tool calls it makes;
//! [`tools`] holds the tools it offers and their dispatch.

mod agent;
pub mod tools;

pub use agent::{Agent, Error, DEFAULT_MAX_TURNS};
