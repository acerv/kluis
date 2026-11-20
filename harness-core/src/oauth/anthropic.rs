// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Claude Pro/Max login.

use genai::resolver::AuthData;
use genai::Headers;

use super::flow::{Flow, FlowConfig, Token};
use super::http::BodyFormat;
use super::store::OAuthFile;
use super::{OAuthProvider, Provider, Tokens};
use crate::Error;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTH_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &[&str] = &["org:create_api_key", "user:profile", "user:inference"];
const API_URL: &str = "https://api.anthropic.com/v1/";
const API_VERSION: &str = "2023-06-01";
/// Beta flag enabling OAuth access tokens on the Messages API.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Anthropic login settings.
#[derive(Debug, Clone)]
pub struct Anthropic {
    flow: FlowConfig,
}

impl Default for Anthropic {
    fn default() -> Self {
        Self::new(FlowConfig {
            client_id: CLIENT_ID.into(),
            token_url: TOKEN_URL.into(),
            scopes: SCOPES.iter().map(|&s| s.into()).collect(),
            // Ask the callback page to show the code for pasting.
            auth_params: vec![("code".into(), "true".into())],
            body: BodyFormat::Json,
            flow: Flow::Paste {
                auth_url: AUTH_URL.into(),
                redirect_uri: REDIRECT_URI.into(),
            },
        })
    }
}

impl Anthropic {
    /// Anthropic login through custom `flow` settings.
    pub fn new(flow: FlowConfig) -> Self {
        Self { flow }
    }
}

impl Provider for Anthropic {
    type Credentials = Tokens;

    const ID: OAuthProvider = OAuthProvider::Anthropic;

    fn flow(&self) -> &FlowConfig {
        &self.flow
    }

    fn slot<'a>(&self, file: &'a mut OAuthFile) -> &'a mut Option<Tokens> {
        &mut file.anthropic
    }

    async fn login(&self, token: &Token) -> Result<Tokens, Error> {
        Ok(Tokens::from_token(token, None))
    }

    fn expires_soon(&self, creds: &Tokens) -> bool {
        creds.expires_soon()
    }

    async fn refresh(&self, creds: &Tokens) -> Result<Tokens, Error> {
        creds.refresh(Self::ID, &self.flow).await
    }

    fn access_token<'a>(&self, creds: &'a Tokens) -> &'a str {
        &creds.access_token
    }

    fn auth(&self, creds: &Tokens, endpoint: Option<&str>) -> AuthData {
        AuthData::RequestOverride {
            url: format!("{}messages", endpoint.unwrap_or(API_URL)),
            headers: headers(&creds.access_token),
        }
    }
}

/// Headers authenticating an API request with `access_token`.
fn headers(access_token: &str) -> Headers {
    Headers::from([
        ("authorization", format!("Bearer {access_token}")),
        ("anthropic-version", API_VERSION.to_owned()),
        ("anthropic-beta", OAUTH_BETA.to_owned()),
    ])
}
