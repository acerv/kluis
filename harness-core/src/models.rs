// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Live model discovery across configured providers.

use std::time::Duration;

use futures_util::future::join_all;
use genai::adapter::AdapterKind;
use genai::resolver::AuthData;
use genai::{Client, Headers};
use serde_json::{json, Value};

use crate::agent::Thinking;
use crate::oauth::OAuthLogins;

/// Per-provider timeout when querying live `/models` endpoints.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

/// Timeout when asking a provider which thinking levels a model takes.
const THINKING_TIMEOUT: Duration = Duration::from_secs(3);

/// `anthropic-version` header of Anthropic API requests.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// One chat-capable model discovered from a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Model identifier suitable for [`crate::Agent::set_model`] (namespaced
    /// with `<provider>::` whenever `genai` would not auto-route it to
    /// `provider`).
    pub id: String,
    /// Lowercase provider name (e.g. `"anthropic"`, `"openai"`, `"deepseek"`, `"genai_1"`).
    pub provider: String,
}

/// Outcome of querying configured providers for available models.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModelList {
    pub models: Vec<ModelInfo>,
    /// Non-fatal per-provider errors (`(provider, message)`).
    pub errors: Vec<(String, String)>,
}

/// Query every adapter in `adapters` concurrently for its available chat
/// models, filtering out obvious non-chat models (embeddings, audio, image,
/// moderation) and auto-namespacing models when needed for `genai` routing.
/// Adapters covered by `oauth` are queried through their login.
pub async fn discover_models(
    client: &Client,
    oauth: Option<&OAuthLogins>,
    adapters: &[AdapterKind],
) -> ModelList {
    discover_models_with_timeout(client, oauth, adapters, DISCOVERY_TIMEOUT).await
}

/// Like [`discover_models`], with an explicit per-provider timeout.
pub async fn discover_models_with_timeout(
    client: &Client,
    oauth: Option<&OAuthLogins>,
    adapters: &[AdapterKind],
    timeout: Duration,
) -> ModelList {
    let mut unique = Vec::with_capacity(adapters.len());
    for &kind in adapters {
        if !unique.contains(&kind) {
            unique.push(kind);
        }
    }

    let futs = unique.into_iter().map(|kind| async move {
        let provider = provider_name(kind);
        let names = async {
            match oauth.filter(|o| o.covers(kind)) {
                Some(oauth) => oauth.models(kind).await,
                None => Ok(client.all_model_names(kind, ()).await?),
            }
        };
        match tokio::time::timeout(timeout, names).await {
            Ok(Ok(mut raw)) => {
                raw.retain(|id| is_chat_model(id));
                raw.sort_unstable();
                raw.dedup();
                let models = raw
                    .into_iter()
                    .map(|id| ModelInfo {
                        id: qualify_model_id(kind, &provider, id),
                        provider: provider.clone(),
                    })
                    .collect::<Vec<_>>();
                Ok(models)
            }
            Ok(Err(err)) => Err((provider, err.to_string())),
            Err(_) => Err((provider, "timed out".to_owned())),
        }
    });

    let mut out = ModelList::default();
    for res in join_all(futs).await {
        match res {
            Ok(models) => out.models.extend(models),
            Err(err) => out.errors.push(err),
        }
    }
    out
}

/// Lowercase provider identifier matching `genai`'s namespace syntax.
pub fn provider_name(kind: AdapterKind) -> String {
    match kind {
        AdapterKind::Custom(n) => format!("genai_{n}"),
        _ => kind.as_lower_str().to_owned(),
    }
}

/// Prefix `raw_id` with `<provider>::` only when `AdapterKind::from_model`
/// would not already route it to `kind` (or its OpenAI/Gemini sibling).
fn qualify_model_id(kind: AdapterKind, provider: &str, raw_id: String) -> String {
    if raw_id.contains("::") {
        return raw_id;
    }
    let routed = AdapterKind::from_model(&raw_id).ok();
    let matches_kind = match (kind, routed) {
        (AdapterKind::OpenAI, Some(AdapterKind::OpenAI | AdapterKind::OpenAIResp)) => true,
        (AdapterKind::Gemini, Some(AdapterKind::Gemini | AdapterKind::GeminiIx)) => true,
        (k, Some(r)) => k == r,
        _ => false,
    };
    if matches_kind {
        raw_id
    } else {
        format!("{provider}::{raw_id}")
    }
}

/// Heuristic filter excluding obvious non-chat models (embeddings, audio,
/// image/video generation, moderation, legacy base completions) that some
/// `/models` endpoints include alongside chat models.
pub fn is_chat_model(id: &str) -> bool {
    let leaf = id.rsplit('/').next().unwrap_or(id);
    let leaf = leaf.rsplit("::").next().unwrap_or(leaf).trim();
    if leaf.is_empty() {
        return false;
    }
    let lower = leaf.to_ascii_lowercase();
    const EXCLUDE_CONTAINS: &[&str] = &[
        "embedding",
        "embed-",
        "-embed",
        "whisper",
        "tts-",
        "-tts",
        "transcribe",
        "realtime",
        "-audio",
        "dall-e",
        "gpt-image",
        "imagen",
        "veo-",
        "sora",
        "moderation",
        "davinci",
        "babbage",
        "curie",
        "rerank",
    ];
    const EXCLUDE_EXACT_OR_DASH: &[&str] = &["ada", "aqa", "tts", "veo"];
    const EXCLUDE_PREFIX: &[&str] = &["bge-", "e5-"];
    !EXCLUDE_CONTAINS.iter().any(|pat| lower.contains(pat))
        && !EXCLUDE_PREFIX.iter().any(|pat| lower.starts_with(pat))
        && !EXCLUDE_EXACT_OR_DASH.iter().any(|pat| {
            lower
                .strip_prefix(pat)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
}

/// Thinking levels `model` takes, as its provider reports them: the model
/// capabilities of Anthropic, or `/api/show` of Ollama. `None` when the
/// provider does not report them, or does not answer.
pub async fn thinking_levels(client: &Client, model: &str) -> Option<Vec<Thinking>> {
    let target = client.resolve_service_target(model).await.ok()?;
    let kind = target.model.adapter_kind;
    let name = target.model.model_name.namespace_and_name().1;
    let http = reqwest::Client::new();
    let request = match kind {
        AdapterKind::Anthropic => {
            let (base, headers) = match target.auth {
                // An OAuth login, whose URL is the one of messages.
                AuthData::RequestOverride { url, headers } => {
                    (url.strip_suffix("messages")?.to_owned(), headers)
                }
                auth => (
                    target.endpoint.base_url().to_owned(),
                    Headers::from([
                        ("x-api-key", auth.single_key_value().ok()?),
                        ("anthropic-version", ANTHROPIC_VERSION.to_owned()),
                    ]),
                ),
            };
            let mut request = http.get(format!("{base}models/{name}"));
            for (key, value) in headers.iter() {
                request = request.header(key, value);
            }
            request
        }
        AdapterKind::Ollama | AdapterKind::OllamaCloud => {
            let url = format!("{}api/show", target.endpoint.base_url());
            let body = json!({ "model": name }).to_string();
            let request = http
                .post(url)
                .header("content-type", "application/json")
                .body(body);
            match target.auth.single_key_value() {
                Ok(key) if !key.is_empty() => request.bearer_auth(key),
                _ => request,
            }
        }
        _ => return None,
    };
    let body = tokio::time::timeout(THINKING_TIMEOUT, async {
        request.send().await?.error_for_status()?.bytes().await
    })
    .await
    .ok()?
    .ok()?;
    let body: Value = serde_json::from_slice(&body).ok()?;
    let (info, takes): (&Value, fn(&Value, Thinking) -> bool) = match kind {
        AdapterKind::Anthropic => (
            body.get("capabilities").filter(|caps| !caps.is_null())?,
            anthropic_takes,
        ),
        _ => (body.pointer("/thinking/values")?, ollama_takes),
    };
    Some(
        Thinking::ALL
            .into_iter()
            .filter(|&level| takes(info, level))
            .collect(),
    )
}

/// Whether a model takes `level`, from its Anthropic `capabilities`.
fn anthropic_takes(caps: &Value, level: Thinking) -> bool {
    let supported = |path: &str| {
        caps.pointer(&format!("{path}/supported"))
            .and_then(Value::as_bool)
            == Some(true)
    };
    match level {
        Thinking::Default => true,
        Thinking::Off => supported("/thinking/types/disabled"),
        // Models without effort get a thinking budget for these from genai.
        Thinking::Low | Thinking::Medium | Thinking::High if !supported("/effort") => {
            supported("/thinking")
        }
        _ => supported("/effort") && supported(&format!("/effort/{level}")),
    }
}

/// Whether a model takes `level`, from the `thinking.values` of its Ollama
/// `/api/show`.
fn ollama_takes(values: &Value, level: Thinking) -> bool {
    // What genai sends as `think` for each level.
    let think = match level {
        Thinking::Default => return true,
        Thinking::Off => json!(false),
        // Sent as "max", like `Max`.
        Thinking::XHigh => return false,
        _ => json!(level.as_str()),
    };
    values
        .as_array()
        .is_some_and(|values| values.contains(&think))
}
