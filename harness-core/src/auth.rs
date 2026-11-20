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
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use genai::adapter::AdapterKind;
use genai::resolver::{self, AuthData, AuthResolver, Endpoint, ProviderConfig};
use genai::{Client, ModelIden};
use serde::Deserialize;

use crate::oauth::{OAuthProvider, OAuthStore};
use crate::{private_file, Error};

type ResolverFuture = Pin<Box<dyn Future<Output = resolver::Result<Option<AuthData>>> + Send>>;

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

    /// The OAuth login used for `kind`: one is saved and no API key
    /// environment variable overrides it.
    fn oauth_login(&self, kind: AdapterKind) -> Option<OAuthProvider> {
        let provider = OAuthProvider::from_adapter(kind)?;
        (self.oauth.as_ref()?.is_logged_in(provider) && !env_key_present(kind)).then_some(provider)
    }

    /// Resolver authenticating adapters with an OAuth login, refreshing its
    /// token before each request when needed.
    fn oauth_resolver(&self) -> Option<AuthResolver> {
        let store = self.oauth.clone()?;
        let logins: Arc<[_]> = OAuthProvider::ALL
            .iter()
            .filter_map(|p| {
                let kind = p.adapter();
                let endpoint = self.get(kind).and_then(|e| e.endpoint.clone());
                Some((kind, self.oauth_login(kind)?, endpoint))
            })
            .collect();
        if logins.is_empty() {
            return None;
        }
        Some(AuthResolver::from_resolver_async_fn(
            move |model: ModelIden| -> ResolverFuture {
                let store = store.clone();
                let logins = logins.clone();
                Box::pin(async move {
                    let Some((_, provider, endpoint)) =
                        logins.iter().find(|(k, ..)| *k == model.adapter_kind)
                    else {
                        return Ok(None);
                    };
                    store
                        .auth_data(*provider, endpoint.as_deref())
                        .await
                        .map(Some)
                        .map_err(|e| resolver::Error::Custom(e.to_string()))
                })
            },
        ))
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
        if let Some(resolver) = self.oauth_resolver() {
            builder = builder.with_auth_resolver(resolver);
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

fn env_key_present(kind: AdapterKind) -> bool {
    if let AdapterKind::Custom(n) = kind {
        return std::env::var_os(format!("GENAI_{n}_API_KEY")).is_some_and(|v| !v.is_empty());
    }
    kind.default_key_env_name()
        .is_some_and(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}
