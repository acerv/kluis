// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The tools offered to the model and the dispatch of its calls.

pub use harness_exec::{
    definitions, execute, execute_streaming, is_read_only, run, run_with, BashContext, Diff,
    DiffHunk, DiffLine, PreExec, ToolContext, ToolDetails,
};
