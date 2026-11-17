// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Default sandbox for an agent harness and the commands it runs.

use std::path::{Path, PathBuf};

use crate::Sandbox;

/// Readable by default: programs, the dynamic loader cache, user and group
/// names, the time zone, and the files used for name resolution.
const DEFAULT_READ: &[&str] = &[
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
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/host.conf",
    "/etc/gai.conf",
];

/// Port the harness uses to reach the model.
const HTTPS: u16 = 443;

/// Processes and threads the harness and its commands may run at once.
const MAX_PROCESSES: u64 = 1024;

/// Share of the memory the private `/tmp`, which lives there, may use.
const TMP_MEMORY_PERCENT: u64 = 10;

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

impl Sandbox {
    /// The sandbox of a harness and the commands it runs, for
    /// [`Sandbox::container`].
    ///
    /// It may run programs from `/usr`, read the few `/etc` files commonly
    /// needed, its DNS files, the CA certificates rustls uses and all of
    /// `$HOME`, and change the [`Options::write`] paths. It may connect to
    /// port 443 to reach the model, and to [`Options::connect`], or anywhere
    /// with [`Options::network`]. It may run at most 1024 processes and
    /// threads, and store in `/tmp` at most 10% of the memory.
    pub fn with_defaults(options: &Options) -> Self {
        let mut sandbox = Sandbox::new()
            .max_processes(MAX_PROCESSES)
            .max_tmp_size(total_memory() * TMP_MEMORY_PERCENT / 100);
        for path in DEFAULT_READ
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
        {
            sandbox = sandbox.allow_read(path);
        }
        // Toolchains, caches and settings of the user live there.
        if let Some(home) = std::env::var_os("HOME").filter(|h| Path::new(h).is_dir()) {
            sandbox = sandbox.allow_read(home);
        }
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

/// Physical memory of the machine in bytes.
fn total_memory() -> u64 {
    // SAFETY: sysinfo only fills the struct it is given.
    unsafe {
        let mut info: libc::sysinfo = std::mem::zeroed();
        if libc::sysinfo(&mut info) != 0 {
            // Cannot happen with a valid pointer; 0 would mean no limit.
            return 1 << 30;
        }
        // `totalram` is a `c_ulong`, 32 bits wide on some machines.
        info.totalram as u64 * u64::from(info.mem_unit)
    }
}
