// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Sandboxed tool executor for `kl`: spawns `kl --tool-executor` inside a
//! `hakoniwa` container with the current folder writable (or read-only with
//! `--strict`), `~/.kluis` hidden, zero API keys in the environment, and no
//! network unless `--network` or `--allow-connect` is given.

use std::path::Path;

use harness_core::tools::{Executor, SandboxConfig};
use harness_core::Error;
use harness_sandbox::{Options, Sandbox};

use crate::Args;

/// Spawn the sandboxed tool executor for `args` in `cwd`.
pub fn executor(args: &Args, cwd: &Path) -> Result<Executor, Error> {
    let exe = std::env::current_exe()?;
    let mut read = args.allow_read.clone();
    let mut write = args.allow_write.clone();
    let mut deny = args.deny_read.clone();
    if args.strict {
        read.push(cwd.to_path_buf());
    } else {
        write.push(cwd.to_path_buf());
    }

    // Hide all harness state (`~/.kluis`: config, auth, sessions, history,
    // global AGENTS.md) from sandboxed tools.
    let kluis_dir = crate::config::kluis_dir();
    if let Some(dir) = &kluis_dir {
        deny.push(dir.clone());
    }

    // If `--config` lives inside a writable folder (outside `~/.kluis`), keep
    // it read-only so commands cannot tamper with it.
    let pin_config = args.config.as_ref().filter(|config| {
        if kluis_dir
            .as_ref()
            .is_some_and(|dir| config.starts_with(dir))
        {
            return false;
        }
        let canon = std::fs::canonicalize(config).ok();
        write.iter().any(|w| {
            config.starts_with(w)
                || canon
                    .as_deref()
                    .zip(std::fs::canonicalize(w).ok().as_deref())
                    .is_some_and(|(c, cw)| c.starts_with(cw))
        })
    });

    let mut sandbox = Sandbox::with_defaults(&Options {
        read,
        write,
        deny,
        connect: args.allow_connect.clone(),
        network: args.network,
        max_processes: args.max_processes,
        tmp_size: args.tmp_size,
    })
    .allow_read(&exe);
    if let Some(config) = pin_config {
        sandbox = sandbox.keep_read_only(config);
    }

    let mut worker_args = vec!["--tool-executor".to_owned()];
    if let Some(bytes) = args.max_memory {
        worker_args.push("--max-memory".to_owned());
        worker_args.push(bytes.to_string());
    }

    let env = harness_sandbox::command_env(std::env::vars_os(), &args.pass_env)
        .into_iter()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();

    Ok(Executor::sandbox(SandboxConfig {
        sandbox,
        program: exe,
        args: worker_args,
        cwd: cwd.to_path_buf(),
        env,
    })?)
}
