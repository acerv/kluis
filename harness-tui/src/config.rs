// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Config file, `~/.kluis/config.toml` by default:
//!
//! ```toml
//! [sandbox]
//! # Also allow reading, or changing, these paths; `~` is $HOME.
//! read = ["/etc/ssl"]
//! write = ["~/.cache/sccache"]
//! # Hide these paths even inside a readable or writable folder.
//! deny = ["~/.ssh", "~/.gnupg"]
//! # Also let the harness connect to these TCP ports, e.g. for a local model.
//! connect = [11434]
//! # Allow any network, to commands too.
//! network = false
//! # Processes and threads the harness and its commands may run at once.
//! max_processes = 1024
//! # Size of the private /tmp in bytes, or with a K, M or G suffix.
//! tmp_size = "512M"
//! # Virtual memory limit for commands in bytes, or with a K, M or G suffix.
//! max_memory = "4G"
//! # Also pass these environment variables to commands.
//! env = ["CC", "CFLAGS"]
//! ```
//!
//! Lists add up with the matching command-line options, and switches are on
//! if either turns them on.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use harness_core::Error;
use serde::{Deserialize, Deserializer, Serialize};

/// Environment variable naming the config file, like `--config`. Also how
/// the harness tells its copy in the container which file it uses.
pub const ENV: &str = "HARNESS_CONFIG";

/// Settings read from the config file.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

/// The `[sandbox]` section.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// Also readable, like `--allow-read`.
    pub read: Vec<PathBuf>,
    /// Also writable, like `--allow-write`.
    pub write: Vec<PathBuf>,
    /// Hidden from the harness and its commands, like `--deny-read`.
    pub deny: Vec<PathBuf>,
    /// Also connectable TCP ports, like `--allow-connect`.
    pub connect: Vec<u16>,
    /// Allow any network, like `--network`.
    pub network: bool,
    /// Process and thread limit, like `--max-processes`.
    pub max_processes: Option<u64>,
    /// Private `/tmp` size in bytes, like `--tmp-size`.
    #[serde(deserialize_with = "deserialize_opt_size")]
    pub tmp_size: Option<u64>,
    /// Virtual memory limit in bytes, like `--max-memory`.
    #[serde(deserialize_with = "deserialize_opt_size")]
    pub max_memory: Option<u64>,
    /// Also passed to commands, like `--pass-env`.
    pub env: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            read: Vec::new(),
            write: Vec::new(),
            deny: vec!["~/.ssh".into(), "~/.gnupg".into()],
            connect: Vec::new(),
            network: false,
            max_processes: Some(1024),
            tmp_size: None,
            max_memory: None,
            env: Vec::new(),
        }
    }
}

/// Where the config file is looked for when not given:
/// `~/.kluis/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    Some(home()?.join(".kluis").join("config.toml"))
}

/// Where global agent instructions are looked for:
/// `~/.kluis/AGENTS.md`.
pub fn agents_path() -> Option<PathBuf> {
    Some(home()?.join(".kluis").join(harness_core::AGENTS_FILE))
}

/// Read the config file at `path`, creating it with the default template
/// first if `!required` and it does not exist yet. Paths in it come back
/// absolute.
pub fn load(path: &Path, required: bool) -> Result<Config, Error> {
    let error = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound && !required => {
            init_default(path).map_err(|e| error(&e))?;
            std::fs::read_to_string(path).map_err(|e| error(&e))?
        }
        Err(e) => return Err(error(&e).into()),
    };
    let mut config: Config = toml::from_str(&text).map_err(|e| error(&e))?;
    let sandbox = &mut config.sandbox;
    for paths in [&mut sandbox.read, &mut sandbox.write] {
        for path in paths {
            *path = absolute(path).map_err(|e| error(&e))?;
        }
    }
    if home().is_none() {
        sandbox.deny.retain(|p| !p.starts_with("~"));
    }
    for path in &mut sandbox.deny {
        *path = absolute(path).map_err(|e| error(&e))?;
    }
    if sandbox.max_processes == Some(0) {
        return Err(error(&"max_processes: must be at least 1").into());
    }
    Ok(config)
}

/// Create `path` and its parent folders with [`Config::default`] if it does
/// not exist yet.
fn init_default(path: &Path) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(&Config::default())?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file.write_all(text.as_bytes())?,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// `path` with a leading `~` replaced by `$HOME`. Relative paths are
/// refused: there is no obvious folder to resolve them from.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    let path = match path.strip_prefix("~") {
        Ok(rest) => home().ok_or("~: HOME is not set")?.join(rest),
        Err(_) => path.to_path_buf(),
    };
    if !path.is_absolute() {
        return Err(format!("{}: not an absolute path", path.display()));
    }
    Ok(path)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Parse a byte size from a number with an optional `K`, `M` or `G` suffix
/// (powers of 1024). Zero is rejected.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let s = text.trim();
    let (num, unit) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1u64 << 20),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1u64 << 30),
        _ => (s, 1u64),
    };
    let value: u64 = num
        .parse()
        .map_err(|_| format!("{text}: invalid byte size"))?;
    let bytes = value
        .checked_mul(unit)
        .ok_or_else(|| format!("{text}: size is too large"))?;
    if bytes == 0 {
        return Err("size must be at least 1".to_string());
    }
    Ok(bytes)
}

fn deserialize_opt_size<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawSize {
        Bytes(u64),
        Text(String),
    }

    match RawSize::deserialize(deserializer)? {
        RawSize::Bytes(n) => parse_size(&n.to_string()),
        RawSize::Text(s) => parse_size(&s),
    }
    .map(Some)
    .map_err(serde::de::Error::custom)
}
