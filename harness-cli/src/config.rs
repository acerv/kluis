// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Config file, `~/.config/harness/config.toml` by default:
//!
//! ```toml
//! [sandbox]
//! # Also pass these environment variables to commands.
//! env = ["CC", "CFLAGS"]
//! ```
//!
//! Lists add up with the matching command-line options.

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
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|home| home.join(".config"))
        })?;
    Some(base.join("harness").join("config.toml"))
}

/// Read the config file at `path`, or `None` if it is missing and not
/// `required`.
pub fn load(path: &Path, required: bool) -> Result<Option<Config>, Error> {
    let error = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound && !required => return Ok(None),
        Err(e) => return Err(error(&e).into()),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| error(&e).into())
}
