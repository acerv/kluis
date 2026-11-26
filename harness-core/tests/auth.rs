// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::os::unix::fs::PermissionsExt;

use genai::adapter::AdapterKind;
use harness_core::{Agent, AuthConfig, CredentialSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn load_missing_file_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let auth = AuthConfig::load(&dir.path().join("auth.toml")).unwrap();
    assert!(auth.is_empty());
}

#[test]
fn load_rejects_open_permissions_and_accepts_0600() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.toml");
    std::fs::write(&path, "[anthropic]\napi_key = \"sk-ant-test\"\n").unwrap();

    // 0644 must be rejected.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = AuthConfig::load(&path).unwrap_err().to_string();
    assert!(err.contains("chmod 600"), "{err}");

    // 0600 succeeds and Debug redacts the key.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let auth = AuthConfig::load(&path).unwrap();
    let entry = auth.get(AdapterKind::Anthropic).expect("anthropic");
    assert_eq!(entry.api_key.as_deref(), Some("sk-ant-test"));
    let dbg = format!("{entry:?}");
    assert!(dbg.contains("REDACTED"), "{dbg}");
    assert!(!dbg.contains("sk-ant-test"), "{dbg}");
}

#[test]
fn parse_validates_providers_keys_and_normalizes_endpoints() {
    let auth = AuthConfig::parse(
        r#"
[openai]
api_key = "  sk-openai  "

[ollama]
endpoint = "http://127.0.0.1:11434"

[genai_1]
api_key = "custom-key"
endpoint = "http://127.0.0.1:8000/v1/"
"#,
    )
    .unwrap();

    assert_eq!(
        auth.get(AdapterKind::OpenAI)
            .and_then(|e| e.api_key.as_deref()),
        Some("sk-openai")
    );
    assert_eq!(
        auth.get(AdapterKind::Ollama)
            .and_then(|e| e.endpoint.as_deref()),
        Some("http://127.0.0.1:11434/")
    );
    assert_eq!(
        auth.get(AdapterKind::Custom(1))
            .and_then(|e| e.endpoint.as_deref()),
        Some("http://127.0.0.1:8000/v1/")
    );

    assert!(AuthConfig::parse("[not_a_provider]\napi_key = \"x\"\n")
        .unwrap_err()
        .contains("unknown provider"));
    assert!(AuthConfig::parse("[openai]\napi_key = \"   \"\n")
        .unwrap_err()
        .contains("must not be empty"));
    assert!(AuthConfig::parse("[openai]\n")
        .unwrap_err()
        .contains("expected `api_key` or `endpoint`"));
    assert!(AuthConfig::parse("[openai]\nextra = true\n").is_err());
}

#[test]
fn parse_errors_hide_the_keys() {
    for text in [
        "[anthropic]\napi_key = sk-ant-SECRET\n",
        "[anthropic]\napikey = \"sk-ant-SECRET\"\n",
        "anthropic = \"sk-ant-SECRET\"\n",
    ] {
        let err = AuthConfig::parse(text).unwrap_err();
        assert!(!err.contains("SECRET"), "{err}");
        assert!(err.starts_with("line "), "{err}");
    }
    let err = AuthConfig::parse("[anthropic]\napikey = \"x\"\n").unwrap_err();
    assert!(
        err.starts_with("line 2, column 1: unknown field `apikey`"),
        "{err}"
    );
}

fn env_unset(kind: AdapterKind) -> bool {
    kind.default_key_env_name()
        .is_none_or(|name| std::env::var_os(name).is_none_or(|v| v.is_empty()))
}

#[test]
fn credential_source_follows_client_precedence() {
    let auth = AuthConfig::parse(
        "[mistral]\napi_key = \"sk-m\"\n[openai]\napi_key = \"sk-o\"\n[genai_3]\nendpoint = \"http://127.0.0.1:9/v1\"\n",
    )
    .unwrap();
    let source = |kind| auth.credential_source(kind);

    if env_unset(AdapterKind::Mistral) {
        assert_eq!(source(AdapterKind::Mistral), CredentialSource::AuthToml);
    }
    // `[openai]` also covers its Responses API sibling.
    if env_unset(AdapterKind::OpenAIResp) {
        assert_eq!(source(AdapterKind::OpenAIResp), CredentialSource::AuthToml);
    }
    if env_unset(AdapterKind::Groq) {
        assert_eq!(source(AdapterKind::Groq), CredentialSource::Missing);
        assert_eq!(source(AdapterKind::Groq).to_string(), "no credentials");
    }
    if std::env::var_os("GENAI_3_API_KEY").is_none() {
        assert_eq!(source(AdapterKind::Custom(3)), CredentialSource::NotNeeded);
    }
    assert_eq!(
        CredentialSource::Env("MISTRAL_API_KEY".into()).to_string(),
        "MISTRAL_API_KEY"
    );
    assert_eq!(CredentialSource::AuthToml.to_string(), "auth.toml");
}

#[test]
fn configured_adapters_includes_auth_entries_without_duplicate_siblings() {
    let auth = AuthConfig::parse(
        r#"
[openai]
api_key = "sk-1"

[openai_resp]
api_key = "sk-2"

[deepseek]
api_key = "sk-3"

[genai_2]
endpoint = "http://127.0.0.1:9000/v1"
"#,
    )
    .unwrap();

    let adapters = auth.configured_adapters();
    assert!(adapters.contains(&AdapterKind::OpenAI), "{adapters:?}");
    assert!(!adapters.contains(&AdapterKind::OpenAIResp), "{adapters:?}");
    assert!(adapters.contains(&AdapterKind::DeepSeek), "{adapters:?}");
    assert!(adapters.contains(&AdapterKind::Custom(2)), "{adapters:?}");

    // When only a sibling adapter (`[gemini_ix]`) is configured and `GEMINI_API_KEY`
    // is unset, `configured_adapters()` must return `GeminiIx` so discovery uses its auth.
    if std::env::var_os("GEMINI_API_KEY").is_none_or(|v| v.is_empty()) {
        let only_ix = AuthConfig::parse("[gemini_ix]\napi_key = \"sk-ix\"\n").unwrap();
        let ix_adapters = only_ix.configured_adapters();
        assert!(
            ix_adapters.contains(&AdapterKind::GeminiIx),
            "{ix_adapters:?}"
        );
        assert!(
            !ix_adapters.contains(&AdapterKind::Gemini),
            "{ix_adapters:?}"
        );
    }
}

#[tokio::test]
async fn client_authenticates_from_auth_toml_without_env_vars() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).into_owned();

        let body = concat!(
            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,",
            "\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},",
            "\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":0,",
            "\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{},",
            "\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).await.unwrap();
        req
    });

    // Use an adapter whose env var is unlikely to be set (`AdapterKind::Mistral`
    // with explicit namespace and local endpoint).
    let auth = AuthConfig::parse(&format!(
        "[mistral]\napi_key = \"sk-from-auth-toml\"\nendpoint = \"http://{addr}/v1\"\n"
    ))
    .unwrap();
    let client = auth.client().unwrap();
    let mut agent = Agent::with_client(client, "mistral::mistral-small");
    let answer = agent.prompt("hi", |_| {}).await.unwrap();
    assert_eq!(answer, "ok");

    let captured = server.await.unwrap();
    let lower = captured.to_ascii_lowercase();
    assert!(
        lower.contains("authorization: bearer sk-from-auth-toml"),
        "{captured}"
    );
}
