// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Default sandboxes for an agent harness and the commands it runs.

use std::path::PathBuf;

use tempfile::TempDir;

use crate::{Sandbox, SandboxError};

/// Read by commands and the harness: programs, the dynamic loader cache,
/// user and group names, and the time zone.
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
];

/// Devices read by commands, for random numbers.
const COMMAND_DEVICES: &[&str] = &["/dev/urandom", "/dev/zero"];

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

/// What to allow on top of the defaults. Paths must exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Readable by the harness and its commands.
    pub read: Vec<PathBuf>,
    /// Writable by the harness and its commands.
    pub write: Vec<PathBuf>,
    /// TCP ports the harness may connect to besides 443.
    pub connect: Vec<u16>,
    /// Allow any network to the harness and its commands.
    pub network: bool,
}

/// Sandboxes for a harness and the commands it runs.
///
/// Commands may run programs from `/usr`, read the few `/etc` files they
/// commonly need, and change the [`Options::write`] paths and a private
/// temporary folder; they get no network unless [`Options::network`].
/// The harness may do the same, plus read its DNS files and the CA
/// certificates rustls uses, and connect to port 443 to reach the model.
///
/// Commands inherit the harness sandbox, so apply [`Profile::commands`]
/// to them too, e.g. with [`Sandbox::prepare`] before `exec`.
#[derive(Debug)]
pub struct Profile {
    pub harness: Sandbox,
    pub commands: Sandbox,
    /// Private temporary folder, removed when dropped; point `TMPDIR` to it.
    pub tmp: TempDir,
}

impl Profile {
    pub fn new(options: &Options) -> Result<Self, SandboxError> {
        let tmp = tempfile::Builder::new()
            .prefix("harness-")
            .tempdir()
            .map_err(|e| SandboxError {
                message: format!("temporary folder: {e}"),
            })?;

        let commands = allow_existing(Sandbox::new(), COMMAND_READ);
        let mut commands = allow_existing(commands, COMMAND_DEVICES).allow_write(tmp.path());
        for path in &options.read {
            commands = commands.allow_read(path);
        }
        for path in &options.write {
            commands = commands.allow_write(path);
        }

        let mut harness = allow_existing(commands.clone(), HARNESS_READ);
        // The certificates rustls loads, found the same way it finds them,
        // including SSL_CERT_FILE and SSL_CERT_DIR.
        let certs = openssl_probe::probe();
        for path in certs.cert_file.into_iter().chain(certs.cert_dir) {
            harness = harness.allow_read(path);
        }

        if options.network {
            harness = harness.allow_any_network();
            commands = commands.allow_any_network();
        } else {
            harness = harness.allow_connect(HTTPS);
            for &port in &options.connect {
                harness = harness.allow_connect(port);
            }
        }

        Ok(Self {
            harness,
            commands,
            tmp,
        })
    }
}

impl Sandbox {
    /// The sandbox of a harness and the commands it runs, for
    /// [`Sandbox::container`].
    ///
    /// It may run programs from `/usr`, read the few `/etc` files commonly
    /// needed, its DNS files and the CA certificates rustls uses, and change
    /// the [`Options::write`] paths. It may connect to port 443 to reach the
    /// model, and to [`Options::connect`], or anywhere with
    /// [`Options::network`].
    pub fn with_defaults(options: &Options) -> Self {
        let mut sandbox = allow_existing(Sandbox::new(), COMMAND_READ);
        sandbox = allow_existing(sandbox, HARNESS_READ);
        // The certificates rustls loads, found the same way it finds them,
        // including SSL_CERT_FILE and SSL_CERT_DIR.
        let certs = openssl_probe::probe();
        for path in certs.cert_file.into_iter().chain(certs.cert_dir) {
            sandbox = sandbox.allow_read(path);
        }
        for path in &options.read {
            sandbox = sandbox.allow_read(path);
        }
        for path in &options.write {
            sandbox = sandbox.allow_write(path);
        }
        if options.network {
            return sandbox.allow_any_network();
        }
        sandbox = sandbox.allow_connect(HTTPS);
        for &port in &options.connect {
            sandbox = sandbox.allow_connect(port);
        }
        sandbox
    }
}

/// Allow reading the paths of `list` that exist on this system.
fn allow_existing(mut sandbox: Sandbox, list: &[&str]) -> Sandbox {
    for path in list.iter().map(PathBuf::from).filter(|p| p.exists()) {
        sandbox = sandbox.allow_read(path);
    }
    sandbox
}
