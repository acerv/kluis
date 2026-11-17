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

/// Exit code of a container that failed to start.
const CONTAINER_FAILURE: i32 = 125;

/// Whether this process already runs inside the container.
pub fn inside() -> bool {
    std::env::var_os(MARKER).is_some()
}

/// How tools run inside the container: commands lose the network unless
/// `--network` is given.
pub fn tools(args: &Args) -> ToolContext {
    if args.network {
        return ToolContext::default();
    }
    let no_network = NoNetwork::new();
    ToolContext {
        bash: BashContext {
            pre_exec: Some(Arc::new(move || no_network.apply())),
            ..BashContext::default()
        },
    }
}

/// Run this program again, with the same arguments, inside the container,
/// and return its exit code.
pub fn run(args: &Args) -> Result<ExitCode, Error> {
    let cwd = std::env::current_dir()?;
    let exe = std::env::current_exe()?;
    let mut read = args.allow_read.clone();
    let mut write = args.allow_write.clone();
    if args.strict {
        read.push(cwd.clone());
    } else {
        write.push(cwd.clone());
    }
    let container = Sandbox::with_defaults(&Options {
        read,
        write,
        connect: args.allow_connect.clone(),
        network: args.network,
    })
    .allow_read(&exe)
    .container()?;

    let argv = std::env::args_os()
        .skip(1)
        .map(utf8)
        .collect::<Result<Vec<_>, _>>()?;
    // Variables that are not UTF-8 cannot be passed on.
    let env = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
    let status = container
        .command(&utf8(exe.into_os_string())?)
        .args(argv)
        .envs(env)
        .env(MARKER, "1")
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("sandbox: {e}"))?;

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
