// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Config file, `~/.config/harness/config.toml` by default:
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
//! # Also pass these environment variables to commands.
//! env = ["CC", "CFLAGS"]
//! ```
//!
//! Lists add up with the matching command-line options, and switches are on
//! if either turns them on.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use harness_core::Error;
use serde::Deserialize;

/// Environment variable naming the config file, like `--config`. Also how
/// the harness tells its copy in the container which file it uses.
pub const ENV: &str = "HARNESS_CONFIG";

/// Settings read from the config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

/// The `[sandbox]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Also readable, like `--allow-read`.
    #[serde(default)]
    pub read: Vec<PathBuf>,
    /// Also writable, like `--allow-write`.
    #[serde(default)]
    pub write: Vec<PathBuf>,
    /// Hidden from the harness and its commands, like `--deny-read`.
    #[serde(default)]
    pub deny: Vec<PathBuf>,
    /// Also connectable TCP ports, like `--allow-connect`.
    #[serde(default)]
    pub connect: Vec<u16>,
    /// Allow any network, like `--network`.
    #[serde(default)]
    pub network: bool,
    /// Also passed to commands, like `--pass-env`.
    #[serde(default)]
    pub env: Vec<String>,
}

/// Where the config file is looked for when not given:
/// `$XDG_CONFIG_HOME/harness/config.toml`, or `~/.config/harness/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home().map(|home| home.join(".config")))?;
    Some(base.join("harness").join("config.toml"))
}

/// Read the config file at `path`, or `None` if it is missing and not
/// `required`. Paths in it come back absolute.
pub fn load(path: &Path, required: bool) -> Result<Option<Config>, Error> {
    let error = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound && !required => return Ok(None),
        Err(e) => return Err(error(&e).into()),
    };
    let mut config: Config = toml::from_str(&text).map_err(|e| error(&e))?;
    let sandbox = &mut config.sandbox;
    for paths in [&mut sandbox.read, &mut sandbox.write, &mut sandbox.deny] {
        for path in paths {
            *path = absolute(path).map_err(|e| error(&e))?;
        }
    }
    Ok(Some(config))
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
