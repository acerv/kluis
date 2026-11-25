// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! OAuth login flows shared by all providers.
//!
//! [`start`] begins a login and returns a [`Login`] telling the user what to
//! do; finishing it yields a [`Token`]. [`refresh`] renews a token later.

use std::error::Error as StdError;
use std::time::Duration;

use oauth2::basic::{BasicClient, BasicTokenResponse};
use oauth2::url::{form_urlencoded, Url};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, DeviceAuthorizationUrl, EndpointNotSet,
    EndpointSet, ErrorResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken,
    RequestTokenError, Scope, StandardDeviceAuthorizationResponse, TokenUrl,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::http::{BodyFormat, HttpClient};
use crate::error::chain;
use crate::Error;

/// Tokens returned by a successful login or refresh.
pub type Token = BasicTokenResponse;

/// How long a localhost callback login waits for the browser.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum size of the request line sent to the localhost callback.
const MAX_REQUEST_LINE: u64 = 8 * 1024;

/// How long one localhost callback connection may take to send its request,
/// so idle browser preconnects do not block the redirect.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How the user grants access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// Authorization code with PKCE; the provider page at `redirect_uri`
    /// shows a code that the user pastes back.
    Paste {
        auth_url: String,
        redirect_uri: String,
    },
    /// Authorization code with PKCE; the browser is sent back to a local
    /// server on `127.0.0.1:port` at `path` (port 0 picks a free one).
    Callback {
        auth_url: String,
        port: u16,
        path: String,
    },
    /// Device authorization grant (RFC 8628): the user enters a code on a
    /// provider page while we poll for the token.
    Device { device_url: String },
}

/// Provider settings for a login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowConfig {
    pub client_id: String,
    pub token_url: String,
    pub scopes: Vec<String>,
    /// Extra parameters added to the authorization URL.
    pub auth_params: Vec<(String, String)>,
    /// Send the PKCE verifier as `state` instead of a random value, for
    /// providers that expect it.
    pub verifier_as_state: bool,
    pub body: BodyFormat,
    pub flow: Flow,
}

type AuthCodeClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;
type DeviceClient =
    BasicClient<EndpointNotSet, EndpointSet, EndpointNotSet, EndpointNotSet, EndpointSet>;
type TokenClient =
    BasicClient<EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

impl FlowConfig {
    fn client(&self) -> Result<TokenClient, Error> {
        Ok(BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_token_uri(TokenUrl::new(self.token_url.clone())?))
    }

    fn scopes(&self) -> impl Iterator<Item = Scope> + '_ {
        self.scopes.iter().cloned().map(Scope::new)
    }
}

/// A login waiting for the user.
#[derive(Debug)]
pub enum Login {
    Paste(PasteLogin),
    Callback(CallbackLogin),
    Device(DeviceLogin),
}

/// Begin a login with `config`.
pub async fn start(config: &FlowConfig) -> Result<Login, Error> {
    let http = HttpClient::new(config.body)?;
    match &config.flow {
        Flow::Paste {
            auth_url,
            redirect_uri,
        } => {
            let code = AuthCode::new(config, http, auth_url, redirect_uri.clone())?;
            Ok(Login::Paste(PasteLogin { code }))
        }
        Flow::Callback {
            auth_url,
            port,
            path,
        } => {
            let listener = TcpListener::bind(("127.0.0.1", *port)).await?;
            let port = listener.local_addr()?.port();
            let redirect_uri = format!("http://localhost:{port}{path}");
            let code = AuthCode::new(config, http, auth_url, redirect_uri)?;
            Ok(Login::Callback(CallbackLogin {
                code,
                listener,
                path: path.clone(),
            }))
        }
        Flow::Device { device_url } => {
            let client = config
                .client()?
                .set_device_authorization_url(DeviceAuthorizationUrl::new(device_url.clone())?);
            let details = client
                .exchange_device_code()
                .add_scopes(config.scopes())
                .request_async(&http)
                .await
                .map_err(token_error)?;
            Ok(Login::Device(DeviceLogin {
                client,
                http,
                details,
            }))
        }
    }
}

/// Renew an access token with `refresh_token`.
pub async fn refresh(config: &FlowConfig, refresh_token: &str) -> Result<Token, Error> {
    refresh_raw(config, refresh_token)
        .await
        .map_err(token_error)
}

/// Like [`refresh`], keeping the `oauth2` error for callers that inspect it.
pub(crate) async fn refresh_raw(
    config: &FlowConfig,
    refresh_token: &str,
) -> Result<Token, RequestTokenError<super::http::HttpError, oauth2::basic::BasicErrorResponse>> {
    let http = HttpClient::new(config.body).map_err(RequestTokenError::Request)?;
    let refresh_token = RefreshToken::new(refresh_token.to_owned());
    config
        .client()
        .map_err(|e| RequestTokenError::Other(e.to_string()))?
        .exchange_refresh_token(&refresh_token)
        .request_async(&http)
        .await
}

/// Authorization code request shared by paste and callback logins.
#[derive(Debug)]
struct AuthCode {
    client: AuthCodeClient,
    http: HttpClient,
    url: Url,
    state: CsrfToken,
    verifier: PkceCodeVerifier,
}

impl AuthCode {
    fn new(
        config: &FlowConfig,
        http: HttpClient,
        auth_url: &str,
        redirect_uri: String,
    ) -> Result<Self, Error> {
        let client = config
            .client()?
            .set_auth_uri(AuthUrl::new(auth_url.to_owned())?)
            .set_redirect_uri(RedirectUrl::new(redirect_uri)?);
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let state = if config.verifier_as_state {
            CsrfToken::new(verifier.secret().clone())
        } else {
            CsrfToken::new_random()
        };
        let mut request = client
            .authorize_url(|| state)
            .add_scopes(config.scopes())
            .set_pkce_challenge(challenge);
        for (name, value) in &config.auth_params {
            request = request.add_extra_param(name.as_str(), value.as_str());
        }
        let (url, state) = request.url();
        Ok(Self {
            client,
            http,
            url,
            state,
            verifier,
        })
    }

    /// Exchange `code` for a token after checking the returned `state`.
    ///
    /// `state` is also sent to the token endpoint: some providers require it,
    /// and RFC 6749 servers ignore unknown parameters.
    async fn exchange(self, code: &str, state: Option<&str>) -> Result<Token, Error> {
        let expected = self.state.secret();
        if state.is_some_and(|s| s != expected) {
            return Err("login failed: state mismatch".into());
        }
        if code.is_empty() {
            return Err("login failed: empty authorization code".into());
        }
        self.client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .set_pkce_verifier(self.verifier)
            .add_extra_param("state", expected.clone())
            .request_async(&self.http)
            .await
            .map_err(token_error)
    }
}

/// Login where the user pastes back the code shown by the provider.
#[derive(Debug)]
pub struct PasteLogin {
    code: AuthCode,
}

impl PasteLogin {
    /// Page the user must open.
    pub fn url(&self) -> &str {
        self.code.url.as_str()
    }

    /// Finish with the pasted `input`: the code, `code#state`, a
    /// `code=…&state=…` query, or the whole redirect URL.
    pub async fn finish(self, input: &str) -> Result<Token, Error> {
        let (code, state) = parse_code(input)?;
        self.code.exchange(&code, state.as_deref()).await
    }
}

/// Split pasted login `input` into its code and optional state.
fn parse_code(input: &str) -> Result<(String, Option<String>), Error> {
    let input = input.trim();
    let query = if input.starts_with("http://") || input.starts_with("https://") {
        Url::parse(input)?.query().unwrap_or_default().to_owned()
    } else if let Some((code, state)) = input.split_once('#') {
        return Ok((code.to_owned(), Some(state.to_owned())));
    } else if input.contains("code=") {
        input.to_owned()
    } else {
        return Ok((input.to_owned(), None));
    };
    let param = |name: &str| {
        form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    Ok((param("code").unwrap_or_default(), param("state")))
}

/// Login where the browser comes back to a local HTTP server.
#[derive(Debug)]
pub struct CallbackLogin {
    code: AuthCode,
    listener: TcpListener,
    path: String,
}

impl CallbackLogin {
    /// Page the user must open.
    pub fn url(&self) -> &str {
        self.code.url.as_str()
    }

    /// Local port receiving the browser redirect.
    pub fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |a| a.port())
    }

    /// Wait for the browser redirect, then exchange its code.
    pub async fn finish(self) -> Result<Token, Error> {
        let wait = async {
            loop {
                let (stream, _) = self.listener.accept().await?;
                if let Ok(res) = tokio::time::timeout(REQUEST_TIMEOUT, self.callback(stream)).await
                {
                    if let Some(params) = res? {
                        return Ok::<_, Error>(params);
                    }
                }
            }
        };
        let (code, state) = tokio::time::timeout(CALLBACK_TIMEOUT, wait)
            .await
            .map_err(|_| "login timed out waiting for the browser")??;
        self.code.exchange(&code, Some(&state)).await
    }

    /// Answer one browser request; return its `code` and `state` when it is
    /// the redirect.
    async fn callback(&self, stream: TcpStream) -> Result<Option<(String, String)>, Error> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        (&mut reader)
            .take(MAX_REQUEST_LINE)
            .read_line(&mut line)
            .await?;
        let target = line.split_whitespace().nth(1).unwrap_or_default();
        let url = Url::parse(&format!("http://localhost{target}"))?;
        if url.path() != self.path {
            respond(reader.get_mut(), "404 Not Found", "Not found.").await;
            return Ok(None);
        }

        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        if let Some(error) = param("error") {
            respond(reader.get_mut(), "400 Bad Request", "Login failed.").await;
            let detail = param("error_description")
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            return Err(format!("login failed: {error}{detail}").into());
        }
        let (Some(code), Some(state)) = (param("code"), param("state")) else {
            respond(reader.get_mut(), "400 Bad Request", "Missing code.").await;
            return Err("login failed: redirect without code or state".into());
        };
        respond(
            reader.get_mut(),
            "200 OK",
            "Login complete. You can close this tab.",
        )
        .await;
        Ok(Some((code, state)))
    }
}

async fn respond(stream: &mut TcpStream, status: &str, text: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    );
    // The browser going away does not change the login result.
    let _ = stream.write_all(resp.as_bytes()).await;
}

/// Login where the user enters a code on a provider page.
#[derive(Debug)]
pub struct DeviceLogin {
    client: DeviceClient,
    http: HttpClient,
    details: StandardDeviceAuthorizationResponse,
}

impl DeviceLogin {
    /// Page the user must open.
    pub fn url(&self) -> &str {
        self.details.verification_uri().as_str()
    }

    /// Code the user must enter on [`Self::url`].
    pub fn user_code(&self) -> &str {
        self.details.user_code().secret()
    }

    /// Poll until the user approves, denies, or the code expires.
    pub async fn finish(self) -> Result<Token, Error> {
        self.client
            .exchange_device_access_token(&self.details)
            .request_async(&self.http, tokio::time::sleep, None)
            .await
            .map_err(token_error)
    }
}

/// Turn an `oauth2` error into a readable message.
pub(crate) fn token_error<RE, T>(err: RequestTokenError<RE, T>) -> Error
where
    RE: StdError + 'static,
    T: ErrorResponse + 'static,
{
    match err {
        RequestTokenError::ServerResponse(resp) => format!("login rejected: {resp}").into(),
        RequestTokenError::Request(err) => format!("request failed: {}", chain(&err)).into(),
        // Neither its body nor the value that does not fit: a response
        // that fails to parse may still hold the tokens.
        RequestTokenError::Parse(err, _) => {
            format!("invalid token response: unexpected `{}`", err.path()).into()
        }
        RequestTokenError::Other(msg) => msg.into(),
    }
}
