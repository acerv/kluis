// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Default sandbox for an agent harness and the commands it runs.

use std::path::{Path, PathBuf};

use crate::Sandbox;

/// Readable by default: programs, the dynamic loader cache, user and group
/// names, the time zone, system release, and the files used for name and
/// service resolution.
const DEFAULT_READ: &[&str] = &[
    "/usr",
    // Real folders on systems without a merged /usr; links to it otherwise.
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc/alternatives",
    "/etc/ld.so.cache",
    "/etc/ld.so.preload",
    "/etc/passwd",
    "/etc/group",
    "/etc/nsswitch.conf",
    "/etc/localtime",
    "/etc/os-release",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/protocols",
    "/etc/services",
];

/// Processes and threads the sandboxed process and its children may run at once.
/// Paths in the home folder that hold credentials, keys or private history,
/// hidden even though the home folder is shown.
const HOME_SECRETS: &[&str] = &[
    // Keys and passwords.
    ".ssh",
    ".gnupg",
    ".password-store",
    ".local/share/keyrings",
    ".netrc",
    // Git hosts.
    ".config/gh",
    ".git-credentials",
    ".config/git/credentials",
    // Cloud.
    ".aws",
    ".config/gcloud",
    ".azure",
    ".kube",
    ".oci",
    // Package registries.
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".npmrc",
    ".pypirc",
    ".gem/credentials",
    ".config/pip",
    // AI tools, whose settings may hold the keys of the servers they run.
    ".claude.json",
    ".claude/.credentials.json",
    ".codex/auth.json",
    ".gemini",
    ".config/github-copilot",
    ".local/share/opencode/auth.json",
    // Containers.
    ".docker/config.json",
    ".config/containers/auth.json",
    // Shell history.
    ".bash_history",
    ".zsh_history",
    ".local/share/fish/fish_history",
    ".python_history",
    // Browsers, mail and chat.
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".thunderbird",
    ".config/Slack",
    ".config/discord",
];

const MAX_PROCESSES: u64 = 1024;

/// A quarter of the memory, the default size of the private `/tmp`.
fn default_tmp_size() -> Option<u64> {
    let total = nix::sys::sysinfo::sysinfo().ok()?.ram_total();
    Some(total / 4).filter(|&bytes| bytes > 0)
}

/// What to allow on top of the defaults. Paths must exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Readable by the sandboxed process and its children.
    pub read: Vec<PathBuf>,
    /// Writable by the sandboxed process and its children.
    pub write: Vec<PathBuf>,
    /// Hidden from the sandboxed process and its children, even below a
    /// readable or writable path, besides the credentials in `$HOME`.
    pub deny: Vec<PathBuf>,
    /// TCP ports the sandboxed process may connect to (empty isolates the
    /// container in an empty network namespace unless [`Options::network`] is
    /// set).
    pub connect: Vec<u16>,
    /// Allow any network to the sandboxed process and its children.
    pub network: bool,
    /// Processes and threads the sandboxed process and its children may run at
    /// once (1024 by default).
    pub max_processes: Option<u64>,
    /// Bytes the private `/tmp` may hold (a quarter of the memory by
    /// default).
    pub tmp_size: Option<u64>,
}

impl Sandbox {
    /// Default sandbox for a tool executor and the commands it runs, for
    /// [`Sandbox::container`].
    ///
    /// It may run programs from `/usr`, read the few `/etc` files commonly
    /// needed, the CA certificates rustls uses, and `$HOME` without the
    /// credentials, keys and history kept there, and change the
    /// [`Options::write`] paths. By default it runs with no network
    /// (`CLONE_NEWNET`); [`Options::connect`] enables Landlock TCP rules for
    /// those ports, and [`Options::network`] allows unrestricted network. It
    /// may run at most 1024 processes and threads (or
    /// [`Options::max_processes`]), and its `/tmp` holds at most a quarter of
    /// the memory (or [`Options::tmp_size`]).
    pub fn with_defaults(options: &Options) -> Self {
        let mut sandbox =
            Sandbox::new().max_processes(options.max_processes.unwrap_or(MAX_PROCESSES));
        // `/tmp` lives in memory, which killing its writers does not free:
        // left unbounded, it could fill the host's memory.
        if let Some(bytes) = options.tmp_size.or_else(default_tmp_size) {
            sandbox = sandbox.max_tmp_size(bytes);
        }
        for path in DEFAULT_READ
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
        {
            sandbox = sandbox.allow_read(path);
        }
        // Toolchains, caches and settings of the user live there.
        if let Some(home) = std::env::var_os("HOME").filter(|h| Path::new(h).is_dir()) {
            let home = PathBuf::from(home);
            for secret in HOME_SECRETS {
                sandbox = sandbox.deny_read(home.join(secret));
            }
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
        for path in &options.deny {
            sandbox = sandbox.deny_read(path);
        }
        if options.network {
            return sandbox.allow_any_network();
        }
        if options.connect.is_empty() {
            return sandbox.no_network();
        }
        for &port in &options.connect {
            sandbox = sandbox.allow_connect(port);
        }
        sandbox
    }
}
