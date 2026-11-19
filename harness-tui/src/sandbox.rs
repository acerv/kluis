// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Sandbox of the harness: it runs again inside the default container of
//! `harness-sandbox`, with the current folder writable (or read-only with
//! `--strict`). Tools and commands share that container, but commands get
//! no network unless `--network` is given.

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;

use harness_core::tools::{BashContext, ToolContext};
use harness_core::Error;
use harness_sandbox::{NoNetwork, Options, Sandbox};

use crate::Args;

/// Set in the environment of the harness running inside the container.
const MARKER: &str = "HARNESS_SANDBOXED";

/// Resolved `AGENTS.md` paths passed to the inner harness process (JSON array).
pub const AGENTS_ENV: &str = "HARNESS_AGENTS";

/// Exit code of a container that failed to start.
const CONTAINER_FAILURE: i32 = 125;

/// Whether this process already runs inside the container.
pub fn inside() -> bool {
    std::env::var_os(MARKER).is_some()
}

/// How tools run inside the container: commands lose the network unless
/// `--network` is given, and get any memory limit.
pub fn tools(args: &Args) -> ToolContext {
    let no_network = (!args.network).then(NoNetwork::new);
    let max_memory = args.max_memory;
    if no_network.is_none() && max_memory.is_none() {
        return ToolContext::default();
    }
    ToolContext {
        bash: BashContext {
            pre_exec: Some(Arc::new(move || {
                if let Some(no_network) = &no_network {
                    no_network.apply()?;
                }
                if let Some(bytes) = max_memory {
                    harness_sandbox::limit_memory(bytes)?;
                }
                Ok(())
            })),
            ..BashContext::default()
        },
        ..ToolContext::default()
    }
}

/// Run this program again, with the same arguments, inside the container,
/// and return its exit code.
pub fn run(args: &Args) -> Result<ExitCode, Error> {
    let cwd = std::env::current_dir()?;
    let exe = std::env::current_exe()?;
    let mut read = args.allow_read.clone();
    let mut write = args.allow_write.clone();
    let mut deny = args.deny_read.clone();
    if args.strict {
        read.push(cwd.clone());
    } else {
        write.push(cwd.clone());
    }
    // Mount only the current project's session folder writable and mask every
    // other project's sessions (since $HOME is mounted read-only by default).
    if let Some(root) = crate::config::sessions_dir() {
        if args.no_session {
            deny.push(root);
        } else {
            let store = harness_core::SessionStore::new(&root, &cwd);
            if !args.continue_session {
                store.ensure_dir()?;
            }
            let project_dir = store.project_dir().to_path_buf();
            if project_dir.exists() {
                write.push(project_dir.clone());
            }
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path != project_dir {
                        deny.push(path);
                    }
                }
            }
        }
    }
    let mut container = Sandbox::with_defaults(&Options {
        read,
        write,
        deny,
        connect: args.allow_connect.clone(),
        network: args.network,
        max_processes: args.max_processes,
        tmp_size: args.tmp_size,
    })
    .allow_read(&exe);
    // The copy inside reads the same config file, wherever it is. Commands
    // must not change it, even in the current folder, as it could give
    // them more next time.
    if let Some(config) = &args.config {
        container = container.keep_read_only(config);
    }
    let global_agents = crate::config::agents_path();
    let agents = harness_core::find_agents_files(&cwd, global_agents.as_deref());
    let canonical_cwd = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let canonical_global = global_agents
        .as_deref()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let mut mounted_agents = Vec::with_capacity(agents.len());
    for path in &agents {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let is_global = canonical_global.as_deref() == Some(canonical.as_path());
        if is_global || !canonical.starts_with(&canonical_cwd) {
            // Keep global and parent-repo AGENTS.md readable inside the
            // container, and read-only so sandboxed commands cannot tamper
            // with them.
            container = container.keep_read_only(&canonical);
        }
        mounted_agents.push(canonical);
    }
    let container = container.container()?;

    let argv = std::env::args_os()
        .skip(1)
        .map(utf8)
        .collect::<Result<Vec<_>, _>>()?;
    // Variables that are not UTF-8 cannot be passed on.
    let env = std::env::vars_os()
        .filter(|(k, _)| k != crate::config::ENV && k != AGENTS_ENV)
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
    let mut command = container.command(&utf8(exe.into_os_string())?);
    command
        .args(argv)
        .envs(env)
        .env(MARKER, "1")
        .env(AGENTS_ENV, &serde_json::to_string(&mounted_agents)?)
        .current_dir(cwd);
    if let Some(config) = &args.config {
        let canonical = std::fs::canonicalize(config)?;
        command.env(crate::config::ENV, &utf8(canonical.into_os_string())?);
    }
    let status = command.status().map_err(|e| format!("sandbox: {e}"))?;

    match status.exit_code {
        Some(code) => Ok(exit_code(code)),
        None if status.code == CONTAINER_FAILURE => Err(format!(
            "sandbox: {} (run with --no-sandbox to skip it)",
            status.reason
        )
        .into()),
        // Killed by a signal: 128 + signal number.
        None => Ok(exit_code(status.code)),
    }
}

fn utf8(s: OsString) -> Result<String, Error> {
    s.into_string()
        .map_err(|s| format!("{}: not UTF-8", s.to_string_lossy()).into())
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
