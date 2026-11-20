// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Subscription logins through OAuth.
//!
//! Each provider describes its login [`flow::FlowConfig`] and the
//! credentials it keeps; [`OAuthStore`] saves them in `oauth.toml` and
//! refreshes them when they expire.

pub mod anthropic;
pub mod flow;
pub mod http;
mod store;

use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use genai::adapter::AdapterKind;
use oauth2::basic::BasicErrorResponseType;
use oauth2::{RequestTokenError, TokenResponse};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Error;
use flow::{FlowConfig, Token};
pub use store::OAuthStore;

/// Refresh access tokens this long before they expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// Providers supporting a subscription login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OAuthProvider {
    Anthropic,
}

impl OAuthProvider {
    pub const ALL: &[Self] = &[Self::Anthropic];

    /// Lowercase name, as in `oauth.toml`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
        }
    }

    /// The `genai` adapter the login authenticates.
    pub fn adapter(self) -> AdapterKind {
        match self {
            Self::Anthropic => AdapterKind::Anthropic,
        }
    }

    /// The provider logging in to `adapter`, if any.
    pub fn from_adapter(adapter: AdapterKind) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.adapter() == adapter)
    }
}

impl fmt::Display for OAuthProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for OAuthProvider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.name().eq_ignore_ascii_case(s))
            .ok_or_else(|| {
                let names: Vec<_> = Self::ALL.iter().map(|p| p.name()).collect();
                format!(
                    "unknown login provider '{s}' (expected {})",
                    names.join(", ")
                )
            })
    }
}

/// Access token, refresh token and expiry of a login.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Expiry as seconds since the Unix epoch; `None` never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"REDACTED")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "REDACTED"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Tokens {
    /// Tokens from a login or refresh `token`, keeping `previous` refresh
    /// token when the server does not rotate it.
    pub fn from_token(token: &Token, previous: Option<&str>) -> Self {
        Self {
            access_token: token.access_token().secret().clone(),
            refresh_token: token
                .refresh_token()
                .map(|t| t.secret().clone())
                .or_else(|| previous.map(str::to_owned)),
            expires_at: token.expires_in().map(|d| now() + d.as_secs()),
        }
    }

    /// Whether the access token expires within [`REFRESH_MARGIN`].
    pub fn expires_soon(&self) -> bool {
        self.expires_at
            .is_some_and(|at| at <= now() + REFRESH_MARGIN.as_secs())
    }

    /// Renew these tokens through `flow`.
    async fn refresh(&self, provider: OAuthProvider, flow: &FlowConfig) -> Result<Self, Error> {
        let Some(refresh_token) = &self.refresh_token else {
            return Err(expired(provider));
        };
        match flow::refresh_raw(flow, refresh_token).await {
            Ok(token) => Ok(Self::from_token(&token, Some(refresh_token))),
            Err(RequestTokenError::ServerResponse(resp))
                if *resp.error() == BasicErrorResponseType::InvalidGrant =>
            {
                Err(expired(provider))
            }
            Err(err) => Err(flow::token_error(err)),
        }
    }
}

fn expired(provider: OAuthProvider) -> Error {
    format!("{provider} login expired; log in again").into()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What a provider needs to log in and keep its login fresh.
pub(crate) trait Provider: Send + Sync {
    /// Credentials kept in `oauth.toml`.
    type Credentials: Clone + Serialize + DeserializeOwned + Send + Sync;

    const ID: OAuthProvider;

    fn flow(&self) -> &FlowConfig;

    /// This provider's table in `oauth.toml`.
    fn slot<'a>(&self, file: &'a mut store::OAuthFile) -> &'a mut Option<Self::Credentials>;

    /// Credentials from a completed login.
    fn login(&self, token: &Token)
        -> impl Future<Output = Result<Self::Credentials, Error>> + Send;

    /// Whether `creds` must be refreshed before use.
    fn expires_soon(&self, creds: &Self::Credentials) -> bool;

    fn refresh(
        &self,
        creds: &Self::Credentials,
    ) -> impl Future<Output = Result<Self::Credentials, Error>> + Send;

    fn access_token<'a>(&self, creds: &'a Self::Credentials) -> &'a str;
}
