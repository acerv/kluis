// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::time::Duration;

use genai::adapter::AdapterKind;
use harness_core::{
    discover_models_with_timeout, is_chat_model, provider_name, AuthConfig, ModelInfo,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn mock_models_server(status: &str, body: String) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let status = status.to_owned();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        }
    });
    addr
}

#[test]
fn is_chat_model_keeps_chat_and_filters_non_chat() {
    for keep in [
        "claude-sonnet-4-5",
        "gpt-4o",
        "o3-mini",
        "gemini-2.5-pro",
        "deepseek-chat",
        "deepseek-reasoner",
        "qwen2.5-coder:32b",
        "openai/gpt-4.1",
    ] {
        assert!(is_chat_model(keep), "expected chat model: {keep}");
    }

    for drop in [
        "text-embedding-3-large",
        "mistral-embed",
        "whisper-1",
        "tts-1-hd",
        "dall-e-3",
        "gpt-image-1",
        "imagen-3.0-generate-002",
        "veo-2.0-generate",
        "omni-moderation-latest",
        "bge-large-en-v1.5",
        "openai/whisper-large-v3",
        "openai::tts",
        "openai::ada",
        "rerank-english-v3.0",
        "   ",
    ] {
        assert!(!is_chat_model(drop), "expected non-chat model: {drop}");
    }
}

#[tokio::test]
async fn discover_models_filters_namespaces_and_tolerates_failures() {
    let openai_addr = mock_models_server(
        "200 OK",
        json!({
            "data": [
                { "id": "gpt-4o" },
                { "id": "o3-mini" },
                { "id": "text-embedding-3-large" },
                { "id": "whisper-1" },
                { "id": "dall-e-3" }
            ]
        })
        .to_string(),
    )
    .await;

    let mistral_addr = mock_models_server(
        "200 OK",
        json!({
            "data": [
                { "id": "mistral-large-latest" },
                { "id": "mistral-embed" }
            ]
        })
        .to_string(),
    )
    .await;

    let broken_addr = mock_models_server("401 Unauthorized", r#"{"error":"bad key"}"#.into()).await;

    // Silent listener that accepts and never responds, to verify per-provider timeout.
    let slow_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_addr = slow_listener.local_addr().unwrap();
    let _slow = tokio::spawn(async move {
        if let Ok((_stream, _)) = slow_listener.accept().await {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let auth = AuthConfig::parse(&format!(
        r#"
[openai]
api_key = "sk-openai"
endpoint = "http://{openai_addr}/v1"

[mistral]
api_key = "sk-mistral"
endpoint = "http://{mistral_addr}/v1"

[groq]
api_key = "sk-bad"
endpoint = "http://{broken_addr}/v1"

[genai_1]
api_key = "sk-slow"
endpoint = "http://{slow_addr}/v1"
"#
    ))
    .unwrap();
    let client = auth.client().unwrap();

    let list = discover_models_with_timeout(
        &client,
        &[
            AdapterKind::OpenAI,
            AdapterKind::OpenAI,
            AdapterKind::Mistral,
            AdapterKind::Groq,
            AdapterKind::Custom(1),
        ],
        Duration::from_millis(200),
    )
    .await;

    assert_eq!(
        list.models,
        vec![
            ModelInfo {
                id: "gpt-4o".into(),
                provider: "openai".into(),
            },
            ModelInfo {
                id: "o3-mini".into(),
                provider: "openai".into(),
            },
            ModelInfo {
                id: "mistral::mistral-large-latest".into(),
                provider: "mistral".into(),
            },
        ]
    );
    assert_eq!(list.errors.len(), 2, "{:?}", list.errors);
    assert!(
        list.errors.iter().any(|(p, _)| p == "groq"),
        "{:?}",
        list.errors
    );
    assert!(
        list.errors
            .iter()
            .any(|(p, msg)| p == "genai_1" && msg.contains("timed out")),
        "{:?}",
        list.errors
    );
    assert_eq!(provider_name(AdapterKind::Custom(1)), "genai_1");
}
