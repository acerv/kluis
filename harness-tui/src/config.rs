// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Config file, `~/.kluis/config.toml` by default:
//!
//! ```toml
//! [generic]
//! # Default model when `--model` / `HARNESS_MODEL` is not set.
//! model = "deepseek-chat"
//! # Default thinking effort: "default", "off", "low", "medium", "high",
//! # "xhigh", or "max"..
//! thinking = "medium"
//! # Auto-compact context at this many prompt tokens (`0` disables it).
//! auto_compact_tokens = 100000
//! # Colors for a "dark" (default) or "light" terminal.
//! theme = "dark"
//! # Colors of code blocks: base16-ocean-dark (default, inspired-github
//! # with the light theme), base16-ocean-light,
//! # base16-eighties-dark, base16-mocha-dark, inspired-github,
//! # solarized-dark, solarized-light, or a TextMate theme file.
//! code_theme = "solarized-dark"
//! # Search the web with this SearXNG instance instead of DuckDuckGo.
//! searxng_url = "https://searx.example.org"
//! # Ring the terminal bell when a prompt that ran for 10 seconds or more
//! # ends or asks a question.
//! notify = false
//!
//! [sandbox]
//! # Also allow reading, or changing, these paths; `~` is $HOME. $HOME
//! # is otherwise an empty private folder showing only these paths and
//! # the current folder, e.g. "~/.cargo" or "~/.gitconfig".
//! read = ["/etc/ssl", "~/.cargo", "~/.rustup"]
//! write = ["~/scratch"]
//! # Also hide these paths, even inside a readable or writable folder:
//! # credentials, keys and history in $HOME are always hidden.
//! deny = ["~/work/secrets"]
//! # Also let the harness connect to these TCP ports, e.g. for a local model.
//! connect = [11434]
//! # Allow any network, to commands too.
//! network = false
//! # Let commands reach only these hosts and their subdomains, through a
//! # proxy kl runs, even without network. Not with `connect`.
//! hosts = ["crates.io", "static.crates.io", "index.crates.io"]
//! # Let web_fetch reach only these hosts and their subdomains, private
//! # addresses included, even without network; any public host when
//! # empty. Commands are not limited by it.
//! web_hosts = ["docs.rs", "github.com"]
//! # Let the model keep commands running in the background across
//! # prompts; a git repository they create may be used by your git before
//! # kl disables it.
//! background_commands = false
//! # Processes and threads the harness and its commands may run at once.
//! max_processes = 1024
//! # Size of the private /tmp in bytes, or with a K, M or G suffix; a
//! # quarter of the memory by default.
//! tmp_size = "512M"
//! # Virtual memory limit for commands in bytes, or with a K, M or G suffix.
//! max_memory = "4G"
//! # Also pass these environment variables to commands.
//! env = ["CC", "CFLAGS"]
//!
//! [permissions]
//! # Tools that run without asking: all of them, unless listed below.
//! allow = ["read", "ls", "find", "grep"]
//! # Tools whose calls wait for you to approve them; not offered when
//! # there is no terminal UI to ask in.
//! ask = ["bash", "write", "edit"]
//! # Tools the model does not get.
//! deny = ["web_search"]
//! ```
//!
//! Lists add up with the matching command-line options, and switches are on
//! if either turns them on.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use harness_core::{Error, Thinking};
use harness_tui::ThemeName;
use serde::{Deserialize, Deserializer, Serialize};

/// Environment variable naming the config file, like `--config`.
pub const ENV: &str = "HARNESS_CONFIG";

/// Model used when neither `--model`, `HARNESS_MODEL` nor the config file
/// names one.
pub const DEFAULT_MODEL: &str = "deepseek-chat";

/// Settings read from the config file.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub generic: GenericConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
}

impl Default for Config {
    /// Settings written to a new config file.
    fn default() -> Self {
        Self {
            generic: GenericConfig {
                model: Some(DEFAULT_MODEL.to_owned()),
                thinking: None,
                theme: None,
                code_theme: None,
                auto_compact_tokens: None,
                searxng_url: None,
                notify: false,
            },
            sandbox: SandboxConfig::default(),
            permissions: PermissionsConfig::default(),
        }
    }
}

/// The `[generic]` section.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GenericConfig {
    /// Default model when `--model` / `HARNESS_MODEL` is not set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Default thinking effort (`default`, `off`, `low` ... `max`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    /// Colors for a dark or light terminal, like `--theme`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeName>,
    /// Colors of code blocks, like `--code-theme`: a built-in theme, or a
    /// TextMate theme file when it holds a `/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_theme: Option<String>,
    /// Prompt-token threshold that triggers auto-compaction (`0` disables it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact_tokens: Option<u64>,
    /// SearXNG instance for web searches, like `--searxng-url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searxng_url: Option<String>,
    /// Ring the bell at the end of long prompts, like `--notify`.
    pub notify: bool,
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
    /// Hosts commands reach through the proxy, like `--allow-host`.
    pub hosts: Vec<String>,
    /// Only hosts `web_fetch` reaches, like `--web-host`.
    pub web_hosts: Vec<String>,
    /// Background commands, like `--background-commands`.
    pub background_commands: bool,
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
            deny: Vec::new(),
            connect: Vec::new(),
            network: false,
            hosts: Vec::new(),
            web_hosts: Vec::new(),
            background_commands: false,
            max_processes: Some(1024),
            tmp_size: None,
            max_memory: None,
            env: Vec::new(),
        }
    }
}

/// The `[permissions]` section: tools by name. A tool in several lists
/// gets the strictest, and the matching command-line options replace what
/// is here tool by tool.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionsConfig {
    /// Run without asking, like `--allow-tool`.
    pub allow: Vec<String>,
    /// Wait for an approval, like `--ask-tool`.
    pub ask: Vec<String>,
    /// Not offered, like `--deny-tool`.
    pub deny: Vec<String>,
}

/// Harness state directory (`~/.kluis`).
pub fn kluis_dir() -> Option<PathBuf> {
    Some(home()?.join(".kluis"))
}

/// Where the config file is looked for when not given:
/// `~/.kluis/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    Some(kluis_dir()?.join("config.toml"))
}

/// Where provider credentials are looked for: `~/.kluis/auth.toml`.
pub fn auth_path() -> Option<PathBuf> {
    Some(kluis_dir()?.join("auth.toml"))
}

/// Where OAuth logins are saved: `~/.kluis/oauth.toml`.
pub fn oauth_path() -> Option<PathBuf> {
    Some(kluis_dir()?.join("oauth.toml"))
}

/// Where global agent instructions are looked for:
/// `~/.kluis/AGENTS.md`.
pub fn agents_path() -> Option<PathBuf> {
    Some(kluis_dir()?.join(harness_core::AGENTS_FILE))
}

/// Where skills are looked for: `~/.kluis/skills`.
pub fn skills_path() -> Option<PathBuf> {
    Some(kluis_dir()?.join("skills"))
}

/// The folder of the skills, [`skills_path`], when it is one and not a
/// symlink, which could lead elsewhere.
pub fn skills_dir() -> Option<PathBuf> {
    let dir = skills_path()?;
    std::fs::symlink_metadata(&dir)
        .is_ok_and(|meta| meta.is_dir())
        .then_some(dir)
}

/// Where custom commands are looked for: `~/.kluis/commands`.
pub fn commands_dir() -> Option<PathBuf> {
    Some(kluis_dir()?.join("commands"))
}

/// Where conversation sessions are stored: `~/.kluis/sessions`.
pub fn sessions_dir() -> Option<PathBuf> {
    Some(kluis_dir()?.join("sessions"))
}

/// Where persistent prompt history is stored: `~/.kluis/history`.
pub fn history_path() -> Option<PathBuf> {
    Some(kluis_dir()?.join("history"))
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
    if let Some(model) = &mut config.generic.model {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return Err(error(&"model: must not be empty").into());
        }
        if trimmed.len() != model.len() {
            *model = trimmed.to_owned();
        }
    }
    if let Some(theme) = config
        .generic
        .code_theme
        .as_mut()
        .filter(|t| t.contains('/'))
    {
        *theme = absolute(Path::new(theme))
            .map_err(|e| error(&e))?
            .to_string_lossy()
            .into_owned();
    }
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
        harness_core::private_file::create_dir(parent)?;
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
