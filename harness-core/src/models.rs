// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Live model discovery across configured providers.

use std::time::Duration;

use futures_util::future::join_all;
use genai::adapter::AdapterKind;
use genai::Client;

/// Per-provider timeout when querying live `/models` endpoints.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

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
pub async fn discover_models(client: &Client, adapters: &[AdapterKind]) -> ModelList {
    discover_models_with_timeout(client, adapters, DISCOVERY_TIMEOUT).await
}

/// Like [`discover_models`], with an explicit per-provider timeout.
pub async fn discover_models_with_timeout(
    client: &Client,
    adapters: &[AdapterKind],
    timeout: Duration,
) -> ModelList {
    let futs = adapters.iter().copied().map(|kind| async move {
        let provider = provider_name(kind);
        match tokio::time::timeout(timeout, client.all_model_names(kind, ())).await {
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
    let lower = id.to_ascii_lowercase();
    let leaf = lower.rsplit('/').next().unwrap_or(&lower);
    if leaf.is_empty() {
        return false;
    }
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
    !EXCLUDE_CONTAINS.iter().any(|pat| leaf.contains(pat))
        && !EXCLUDE_PREFIX.iter().any(|pat| leaf.starts_with(pat))
        && !EXCLUDE_EXACT_OR_DASH.iter().any(|pat| {
            leaf.strip_prefix(pat)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
}
