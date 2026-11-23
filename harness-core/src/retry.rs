// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Retrying model requests that failed for a passing reason: rate limits,
//! overloaded or failing servers, and dropped connections.

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use genai::webc;
use reqwest::header::HeaderMap;
use reqwest::StatusCode;

/// When and how often the agent retries a failed model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt; 0 turns retrying off.
    pub max_retries: u32,
    /// Wait before the first retry, doubling after each one.
    pub base_delay: Duration,
    /// Longest wait. A server asking to wait longer is not retried.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// Five retries, waiting 1, 2, 4, 8 and 16 seconds.
    fn default() -> Self {
        Self {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// How long to wait before retry number `retry` (from 1) after `err`,
    /// with why the request failed, or `None` when retrying cannot help.
    pub(crate) fn delay(
        &self,
        retry: u32,
        err: &(dyn StdError + 'static),
    ) -> Option<(Duration, String)> {
        if retry > self.max_retries {
            return None;
        }
        let failure = classify(err)?;
        let backoff = self
            .base_delay
            .saturating_mul(1 << (retry - 1).min(16))
            .min(self.max_delay);
        let delay = match failure.after {
            Some(after) if after > self.max_delay => return None,
            Some(after) => after,
            None => backoff,
        };
        Some((delay, failure.reason))
    }
}

/// The model's reply stopped before its end, as when the connection drops.
#[derive(Debug)]
pub(crate) struct EndedEarly;

impl fmt::Display for EndedEarly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the model's reply ended early")
    }
}

impl StdError for EndedEarly {}

/// A failure that may pass.
struct Transient {
    reason: String,
    /// How long the server asked to wait.
    after: Option<Duration>,
}

/// Whether `err` may pass on a retry.
fn classify(err: &(dyn StdError + 'static)) -> Option<Transient> {
    if err.is::<EndedEarly>() {
        return transient("connection lost");
    }
    match err.downcast_ref::<genai::Error>()? {
        genai::Error::HttpError {
            status, headers, ..
        } => from_status(*status, Some(headers)),
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            webc::Error::Reqwest(err) => from_reqwest(err),
            _ => from_status(webc_error.status()?, webc_error.headers()),
        },
        genai::Error::ChatResponse { body, .. } => {
            // An error event in the stream, e.g. Anthropic's.
            let kind = body["error"]["type"].as_str().unwrap_or_default();
            matches!(kind, "overloaded_error" | "rate_limit_error" | "api_error").then(|| {
                Transient {
                    reason: kind.to_owned(),
                    after: None,
                }
            })
        }
        // The error behind the broken stream decides, such as an HTTP
        // status; an unknown one means the stream broke off.
        genai::Error::WebStream { error, .. } => {
            let inner: &(dyn StdError + 'static) = error.as_ref();
            if let Some(err) = inner.downcast_ref::<reqwest::Error>() {
                from_reqwest(err)
            } else if inner.is::<genai::Error>() {
                classify(inner)
            } else {
                transient("connection lost")
            }
        }
        _ => None,
    }
}

/// Whether the provider refused `err`'s request for being larger than the
/// model's context window.
pub(crate) fn is_too_large(err: &(dyn StdError + 'static)) -> bool {
    let Some(err) = err.downcast_ref::<genai::Error>() else {
        return false;
    };
    match err {
        genai::Error::HttpError { status, body, .. } => too_large(*status, body),
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            webc::Error::ResponseFailedStatus { status, body, .. } => too_large(*status, body),
            _ => false,
        },
        genai::Error::ChatResponse { body, .. } => mentions_context(&body.to_string()),
        genai::Error::WebStream { error, .. } => {
            let inner: &(dyn StdError + 'static) = error.as_ref();
            is_too_large(inner)
        }
        _ => false,
    }
}

/// Whether a request failing with `status` and `body` was too large.
fn too_large(status: StatusCode, body: &str) -> bool {
    status == StatusCode::PAYLOAD_TOO_LARGE
        || (status == StatusCode::BAD_REQUEST && mentions_context(body))
}

/// Whether an error `body` says the context window is exceeded, in the
/// words of the common providers.
fn mentions_context(body: &str) -> bool {
    const PHRASES: [&str; 7] = [
        "context_length_exceeded",
        "context window",
        "maximum context length",
        "prompt is too long",
        "input token count",
        "too many tokens",
        "request_too_large",
    ];
    let body = body.to_ascii_lowercase();
    PHRASES.iter().any(|phrase| body.contains(phrase))
}

/// Whether a failed HTTP exchange may pass. A server that cannot be
/// reached at all, e.g. with no network, will not be reached on a retry
/// either; one that answered and then broke off may.
fn from_reqwest(err: &reqwest::Error) -> Option<Transient> {
    if err.is_connect() || err.is_builder() {
        return None;
    }
    match err.status() {
        Some(status) => from_status(status, None),
        None => transient("connection lost"),
    }
}

fn transient(reason: &str) -> Option<Transient> {
    Some(Transient {
        reason: reason.to_owned(),
        after: None,
    })
}

/// Whether an HTTP `status` may pass, honoring the retry headers servers
/// send with it. A `Retry-After` given as a date falls back to the backoff.
fn from_status(status: StatusCode, headers: Option<&HeaderMap>) -> Option<Transient> {
    let header = |name| headers?.get(name)?.to_str().ok();
    let retry = match header("x-should-retry") {
        Some("true") => true,
        Some("false") => false,
        // 529 is Anthropic's "overloaded".
        _ => matches!(status.as_u16(), 408 | 409 | 429 | 500..=599),
    };
    if !retry {
        return None;
    }
    let after = header("retry-after-ms")
        .and_then(|ms| ms.parse::<f64>().ok())
        // Not `from_secs_f64`, which panics on values such as "inf".
        .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
        .or_else(|| {
            header("retry-after")
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_secs)
        });
    Some(Transient {
        reason: format!("HTTP {status}"),
        after,
    })
}
