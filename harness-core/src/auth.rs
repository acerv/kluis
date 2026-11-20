// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Provider credentials loaded from `~/.kluis/auth.toml`.
//!
//! Each table is keyed by a lowercase `genai` adapter name (`anthropic`,
//! `openai`, `deepseek`, `gemini`, `groq`, `mistral`, `xai`, `open_router`,
//! `ollama`, `github_copilot`, `genai_1`, …) and accepts `api_key` and/or
//! `endpoint`. Environment variables still take precedence when set, and an
//! OAuth login (see [`crate::oauth`]) takes precedence over `api_key`.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint, ProviderConfig};
use genai::Client;
use serde::Deserialize;

use crate::oauth::{OAuthLogins, OAuthProvider, OAuthStore};
use crate::{private_file, Error};

/// Maximum size of an `auth.toml` file (256 KiB).
const MAX_AUTH_BYTES: usize = 256 * 1024;

/// Credentials and optional custom endpoint for one provider.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuth {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderAuth")
            .field("api_key", &self.api_key.as_ref().map(|_| "REDACTED"))
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// Where the credentials of a provider come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// An API key environment variable, e.g. `ANTHROPIC_API_KEY`.
    Env(String),
    /// A subscription login.
    Login(OAuthProvider),
    /// `api_key` in `auth.toml`.
    AuthToml,
    /// None, and the provider needs none, e.g. a local Ollama.
    NotNeeded,
    /// None, though the provider needs an API key.
    Missing,
}

impl fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => f.write_str(name),
            Self::Login(provider) => f.write_str(provider.label()),
            Self::AuthToml => f.write_str("auth.toml"),
            Self::NotNeeded => f.write_str("no key needed"),
            Self::Missing => f.write_str("no credentials"),
        }
    }
}

/// Provider authentication loaded from `auth.toml`.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    providers: HashMap<AdapterKind, ProviderAuth>,
    oauth: Option<Arc<OAuthStore>>,
}

impl AuthConfig {
    /// Load `auth.toml` from `path`. Returns an empty config when the file does
    /// not exist; rejects non-regular files, oversized files, and permissions
    /// accessible by group or others (`mode & 0o077 != 0`).
    pub fn load(path: &Path) -> Result<Self, Error> {
        match private_file::read(path, MAX_AUTH_BYTES)? {
            Some(raw) => Self::parse(&raw).map_err(|e| format!("{}: {e}", path.display()).into()),
            None => Ok(Self::default()),
        }
    }

    /// Parse `auth.toml` contents without touching the filesystem.
    pub fn parse(toml_str: &str) -> Result<Self, String> {
        let raw: HashMap<String, ProviderAuth> =
            toml::from_str(toml_str).map_err(|e| e.to_string())?;
        let mut providers = HashMap::with_capacity(raw.len());
        for (name, mut entry) in raw {
            let kind = parse_adapter(&name).ok_or_else(|| format!("unknown provider '{name}'"))?;
            if let Some(key) = &mut entry.api_key {
                let trimmed = key.trim();
                if trimmed.is_empty() {
                    return Err(format!("{name}.api_key: must not be empty"));
                }
                if trimmed.len() != key.len() {
                    *key = trimmed.to_owned();
                }
            }
            if let Some(ep) = &mut entry.endpoint {
                let trimmed = ep.trim();
                if trimmed.is_empty() {
                    return Err(format!("{name}.endpoint: must not be empty"));
                }
                if trimmed.len() != ep.len() {
                    *ep = trimmed.to_owned();
                }
                if !ep.ends_with('/') {
                    ep.push('/');
                }
            }
            if entry.api_key.is_none() && entry.endpoint.is_none() {
                return Err(format!("{name}: expected `api_key` or `endpoint`"));
            }
            if providers.insert(kind, entry).is_some() {
                return Err(format!("duplicate provider '{name}'"));
            }
        }
        Ok(Self {
            providers,
            oauth: None,
        })
    }

    /// Also authenticate through the logins saved in `oauth`.
    pub fn with_oauth(mut self, oauth: Arc<OAuthStore>) -> Self {
        self.oauth = Some(oauth);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty() && self.oauth.as_ref().is_none_or(|o| o.providers().is_empty())
    }

    pub fn get(&self, kind: AdapterKind) -> Option<&ProviderAuth> {
        self.providers.get(&kind)
    }

    /// Where the credentials of `kind` come from, in the order
    /// [`Self::client`] uses them.
    pub fn credential_source(&self, kind: AdapterKind) -> CredentialSource {
        let env = env_key_name(kind);
        if let Some(name) = env.as_deref().filter(|name| env_set(name)) {
            return CredentialSource::Env(name.to_owned());
        }
        if let Some(provider) = self.oauth_login(kind) {
            return CredentialSource::Login(provider);
        }
        let sibling = match kind {
            AdapterKind::OpenAIResp => Some(AdapterKind::OpenAI),
            AdapterKind::GeminiIx => Some(AdapterKind::Gemini),
            _ => None,
        };
        let entry = self.get(kind).or_else(|| self.get(sibling?));
        if entry.is_some_and(|e| e.api_key.is_some()) {
            CredentialSource::AuthToml
        } else if env.is_none() || matches!(kind, AdapterKind::Custom(_)) {
            CredentialSource::NotNeeded
        } else {
            CredentialSource::Missing
        }
    }

    /// The OAuth login used for `kind`: one is saved and no API key
    /// environment variable overrides it.
    fn oauth_login(&self, kind: AdapterKind) -> Option<OAuthProvider> {
        let provider = OAuthProvider::from_adapter(kind)?;
        (self.oauth.as_ref()?.is_logged_in(provider) && !env_key_present(kind)).then_some(provider)
    }

    /// OAuth logins in use, with their `auth.toml` endpoint overrides.
    pub fn oauth_logins(&self) -> Option<OAuthLogins> {
        let logins = OAuthProvider::ALL
            .iter()
            .filter_map(|p| {
                let kind = p.adapter();
                let endpoint = self.get(kind).and_then(|e| e.endpoint.clone());
                Some((self.oauth_login(kind)?, endpoint))
            })
            .collect();
        OAuthLogins::new(self.oauth.clone()?, logins)
    }

    /// Build a [`Client`] configured with all entries from this `AuthConfig`.
    ///
    /// Environment variables (`ANTHROPIC_API_KEY`, etc.) take precedence over
    /// `auth.toml` when set and non-empty; otherwise an OAuth login does.
    /// `[openai]` also applies to `OpenAIResp`, and `[gemini]` also applies
    /// to `GeminiIx`.
    pub fn client(&self) -> Result<Client, Error> {
        let mut builder = Client::builder();
        for (&kind, entry) in &self.providers {
            let targets: &[AdapterKind] = match kind {
                AdapterKind::OpenAI => &[AdapterKind::OpenAI, AdapterKind::OpenAIResp],
                AdapterKind::Gemini => &[AdapterKind::Gemini, AdapterKind::GeminiIx],
                _ => &[kind],
            };
            for &target in targets {
                // Skip overriding sibling adapters if explicitly configured.
                if target != kind && self.providers.contains_key(&target) {
                    continue;
                }
                let env_set = env_key_present(target);
                let auth = (!env_set)
                    .then(|| {
                        entry
                            .api_key
                            .as_ref()
                            .map(|k| AuthData::from_single(k.as_str()))
                    })
                    .flatten();
                let endpoint = entry
                    .endpoint
                    .as_ref()
                    .map(|ep| Endpoint::from_owned(ep.as_str()));
                if auth.is_some() || endpoint.is_some() {
                    builder =
                        builder.append_provider_config(target, ProviderConfig { endpoint, auth });
                }
            }
        }
        if let Some(logins) = self.oauth_logins() {
            builder = builder.with_auth_resolver(logins.resolver());
        }
        Ok(builder.build()?)
    }

    /// Return all adapters configured either in `auth.toml` or via their
    /// default environment variable, in deterministic order without duplicate
    /// sibling adapters (`OpenAIResp` / `GeminiIx` are only emitted when their
    /// primary `OpenAI` / `Gemini` sibling is not configured).
    pub fn configured_adapters(&self) -> Vec<AdapterKind> {
        let mut out = Vec::new();
        for &kind in AdapterKind::all() {
            let configured = match kind {
                AdapterKind::OpenAIResp => {
                    !out.contains(&AdapterKind::OpenAI) && self.providers.contains_key(&kind)
                }
                AdapterKind::GeminiIx => {
                    !out.contains(&AdapterKind::Gemini) && self.providers.contains_key(&kind)
                }
                _ => {
                    self.providers.contains_key(&kind)
                        || env_key_present(kind)
                        || self.oauth_login(kind).is_some()
                }
            };
            if configured {
                out.push(kind);
            }
        }
        // Include any explicitly configured custom `genai_<n>` adapters.
        let mut custom: Vec<u8> = self
            .providers
            .keys()
            .filter_map(|k| match k {
                AdapterKind::Custom(n) => Some(*n),
                _ => None,
            })
            .collect();
        for n in 1..=9u8 {
            if std::env::var_os(format!("GENAI_{n}_ENDPOINT")).is_some_and(|v| !v.is_empty())
                && !custom.contains(&n)
            {
                custom.push(n);
            }
        }
        custom.sort_unstable();
        for n in custom {
            out.push(AdapterKind::Custom(n));
        }
        out
    }
}

fn parse_adapter(name: &str) -> Option<AdapterKind> {
    let lower = name.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("genai_") {
        if let Ok(n) = rest.parse::<u8>() {
            if n > 0 {
                return Some(AdapterKind::Custom(n));
            }
        }
        return None;
    }
    AdapterKind::from_lower_str(&lower)
}

/// Environment variable holding the API key of `kind`, if it has one.
fn env_key_name(kind: AdapterKind) -> Option<String> {
    match kind {
        AdapterKind::Custom(n) => Some(format!("GENAI_{n}_API_KEY")),
        _ => kind.default_key_env_name().map(str::to_owned),
    }
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn env_key_present(kind: AdapterKind) -> bool {
    env_key_name(kind).is_some_and(|name| env_set(&name))
}
