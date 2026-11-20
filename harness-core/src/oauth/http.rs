// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! HTTP transport for the `oauth2` crate, built on `reqwest`.

use std::future::Future;
use std::pin::Pin;

use oauth2::http::header::{HeaderValue, CONTENT_TYPE};
use oauth2::url::form_urlencoded;
use oauth2::{AsyncHttpClient, HttpClientError, HttpRequest, HttpResponse};
use serde_json::{Map, Value};

/// Error returned by [`HttpClient`].
pub type HttpError = HttpClientError<reqwest::Error>;

/// How token requests encode their parameters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BodyFormat {
    /// `application/x-www-form-urlencoded`, as RFC 6749 requires.
    #[default]
    Form,
    /// A JSON object of strings, for servers that only accept JSON.
    Json,
}

/// `oauth2` HTTP client that never follows redirects.
#[derive(Debug, Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    body: BodyFormat,
}

impl HttpClient {
    pub fn new(body: BodyFormat) -> Result<Self, HttpError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(Box::new)?;
        Ok(Self { client, body })
    }

    /// The underlying `reqwest` client.
    pub fn reqwest(&self) -> &reqwest::Client {
        &self.client
    }

    /// Re-encode a form `request` body according to [`Self::body`].
    fn encode(&self, mut request: HttpRequest) -> Result<HttpRequest, HttpError> {
        if self.body == BodyFormat::Form || request.body().is_empty() {
            return Ok(request);
        }
        let fields: Map<String, Value> = form_urlencoded::parse(request.body())
            .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
            .collect();
        *request.body_mut() =
            serde_json::to_vec(&fields).map_err(|e| HttpError::Other(e.to_string()))?;
        request
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(request)
    }
}

impl<'c> AsyncHttpClient<'c> for HttpClient {
    type Error = HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, HttpError>> + Send + Sync + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let request = self.encode(request)?.try_into().map_err(Box::new)?;
            let response = self.client.execute(request).await.map_err(Box::new)?;
            let mut builder = oauth2::http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let body = response.bytes().await.map_err(Box::new)?;
            Ok(builder.body(body.to_vec())?)
        })
    }
}
