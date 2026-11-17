// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Sandbox of the harness: the default [`Profile`] of `harness-sandbox`,
//! with the current folder writable (or read-only with `--strict`).

use std::sync::Arc;

use harness_core::tools::{BashContext, ToolContext};
use harness_core::Error;
use harness_sandbox::{Options, Profile};
use tempfile::TempDir;

use crate::Args;

/// The sandbox in force: keep it alive while the harness runs, since it
/// owns the private temporary folder.
pub struct Applied {
    pub tools: ToolContext,
    _tmp: TempDir,
}

/// Restrict the whole process and return the context sandboxing commands.
///
/// Must run before any other thread starts: it also sets `TMPDIR`.
pub fn apply(args: &Args) -> Result<Applied, Error> {
    let cwd = std::env::current_dir()?;
    let mut read = args.allow_read.clone();
    let mut write = args.allow_write.clone();
    if args.strict {
        read.push(cwd);
    } else {
        write.push(cwd);
    }
    let profile = Profile::new(&Options {
        read,
        write,
        connect: args.allow_connect.clone(),
        network: args.network,
    })?;

    // Single-threaded here, so changing the environment is safe; commands
    // inherit it.
    std::env::set_var("TMPDIR", profile.tmp.path());

    // Commands inherit the harness sandbox; their own, applied before
    // exec, takes away the harness-only files and network.
    let commands = profile.commands.prepare()?;
    profile.harness.apply()?;

    Ok(Applied {
        tools: ToolContext {
            bash: BashContext {
                pre_exec: Some(Arc::new(move || commands.restrict_self())),
            },
        },
        _tmp: profile.tmp,
    })
}
