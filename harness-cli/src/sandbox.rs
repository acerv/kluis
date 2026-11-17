// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Default sandbox of the harness.
//!
//! Commands may run programs from `/usr`, read the few `/etc` files they
//! commonly need, and change the current folder and a private temporary
//! folder (`TMPDIR`); they get no network unless `--network` is given.
//! The harness may do the same, plus read its DNS files and the CA
//! certificates found by `openssl-probe` (as rustls does), and connect to
//! TCP port 443 to reach the model.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_core::tools::{BashContext, ToolContext};
use harness_core::Error;
use harness_sandbox::Sandbox;
use tempfile::TempDir;

use crate::Args;

/// Read by commands and the harness: programs, the dynamic loader cache,
/// user and group names, the time zone, and random numbers.
const COMMAND_READ: &[&str] = &[
    "/usr",
    // Real folders on systems without a merged /usr; links to it otherwise.
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc/ld.so.cache",
    "/etc/ld.so.preload",
    "/etc/passwd",
    "/etc/group",
    "/etc/nsswitch.conf",
    "/etc/localtime",
    "/dev/urandom",
    "/dev/zero",
];

/// Read by the harness only, for name resolution.
const HARNESS_READ: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/nsswitch.conf",
];

/// Port the harness uses to reach the model.
const HTTPS: u16 = 443;

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
    let tmp = tempfile::Builder::new().prefix("harness-").tempdir()?;
    // Single-threaded here, so changing the environment is safe; commands
    // inherit it.
    std::env::set_var("TMPDIR", tmp.path());

    let commands = shared(args, tmp.path())?;
    // The certificates rustls loads, found the same way it finds them,
    // including SSL_CERT_FILE and SSL_CERT_DIR.
    let certs = openssl_probe::probe();
    let mut process = allow_existing(commands.clone(), HARNESS_READ);
    for path in certs.cert_file.into_iter().chain(certs.cert_dir) {
        process = process.allow_read(path);
    }

    let process = if args.network {
        process.allow_any_network()
    } else {
        args.allow_connect
            .iter()
            .fold(process.allow_connect(HTTPS), |s, &port| {
                s.allow_connect(port)
            })
    };

    // Commands inherit the harness sandbox; a stricter one applied before
    // exec takes away the harness-only files and, without --network, TCP.
    let commands = if args.network {
        commands.allow_any_network()
    } else {
        commands
    };
    let prepared = commands.prepare()?;

    process.apply()?;
    Ok(Applied {
        tools: ToolContext {
            bash: BashContext {
                pre_exec: Some(Arc::new(move || prepared.restrict_self())),
            },
        },
        _tmp: tmp,
    })
}

/// What commands may use; the harness may use it too.
fn shared(args: &Args, tmp: &Path) -> Result<Sandbox, Error> {
    let mut sandbox = allow_existing(Sandbox::new(), COMMAND_READ)
        .allow_write(std::env::current_dir()?)
        .allow_write(tmp);

    // Paths given on the command line must exist.
    for path in &args.allow_read {
        sandbox = sandbox.allow_read(path);
    }
    for path in &args.allow_write {
        sandbox = sandbox.allow_write(path);
    }
    Ok(sandbox)
}

/// Allow reading the paths of `list` that exist on this system.
fn allow_existing(mut sandbox: Sandbox, list: &[&str]) -> Sandbox {
    for path in list.iter().map(PathBuf::from).filter(|p| p.exists()) {
        sandbox = sandbox.allow_read(path);
    }
    sandbox
}
