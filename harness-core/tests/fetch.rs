// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use common::web::{Reply, WebServer};
use common::{text, MockModel};
use harness_core::tools::{
    FetchOutput, FetchTool, WebClient, FETCH_TOOL, MAX_FETCH_BODY_BYTES, MAX_FETCH_BYTES,
};
use harness_core::Agent;
use serde_json::Value;

fn reply(content_type: &'static str, body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status: 200,
        content_type,
        body: body.into(),
    }
}

fn call(url: String) -> FetchTool {
    FetchTool {
        url,
        raw: false,
        offset: None,
    }
}

/// Serve `reply` once and fetch it with `tool` pointed at the server.
async fn fetch(
    reply: Reply,
    tool: impl FnOnce(String) -> FetchTool,
) -> Result<FetchOutput, String> {
    let server = WebServer::serve(vec![reply]).await;
    // The server is on loopback, which only a listed host may reach.
    let client = WebClient::new(&["127.0.0.1"]).unwrap();
    tool(server.url("/page"))
        .run(Some(&client))
        .await
        .map_err(|e| e.message)
}

const PAGE: &str = r##"<html><head><title>T</title><style>p{}</style></head><body>
<nav><a href="/home">Home</a></nav>
<h1>Title</h1>
<script>alert(1)</script>
<p>See <a href="https://example.com/doc">the docs</a> and <a href="#part">this part</a>.
<a href="">Reload</a> <a href="javascript:go()">now</a></p>
<a href="https://example.com/run"><svg></svg></a>
<img src="data:image/png;base64,AAAA" alt="logo">
</body></html>"##;

#[test]
fn definition() {
    let tool = FetchTool::definition();
    assert_eq!(tool.name.as_ref(), FETCH_TOOL);
    assert_eq!(tool.schema, Some(FetchTool::schema()));
}

#[tokio::test]
async fn html_becomes_markdown() {
    let out = fetch(reply("text/html; charset=utf-8", PAGE), call)
        .await
        .unwrap();
    assert_eq!(
        out.content,
        "T\n\n# Title\n\nSee [the docs](https://example.com/doc) and this part. Reload now"
    );
    assert_eq!(out.total_bytes, out.content.len());
    assert!(!out.truncated);
    assert_eq!(out.next_offset, None);
    assert!(!out.incomplete);
}

#[tokio::test]
async fn raw_keeps_html() {
    let out = fetch(reply("text/html", PAGE), |url| FetchTool {
        raw: true,
        ..call(url)
    })
    .await
    .unwrap();
    assert_eq!(out.content, PAGE);
}

#[tokio::test]
async fn text_is_returned_as_is() {
    for content_type in [
        "text/plain",
        "application/json",
        "application/vnd.api+json",
        "",
    ] {
        let out = fetch(reply(content_type, "{\"a\": \"<b>\"}"), call)
            .await
            .unwrap();
        assert_eq!(out.content, "{\"a\": \"<b>\"}", "{content_type}");
    }
}

#[tokio::test]
async fn charset_is_decoded() {
    let out = fetch(
        reply("text/plain; charset=\"ISO-8859-1\"", b"caf\xe8".to_vec()),
        call,
    )
    .await
    .unwrap();
    assert_eq!(out.content, "cafè");
}

#[tokio::test]
async fn binary_is_refused() {
    let err = fetch(reply("image/png", b"\x89PNG".to_vec()), call)
        .await
        .unwrap_err();
    assert!(err.contains("unsupported content type image/png"), "{err}");

    let err = fetch(reply("text/plain", b"a\0b".to_vec()), call)
        .await
        .unwrap_err();
    assert!(err.contains("binary"), "{err}");
}

#[tokio::test]
async fn http_error() {
    let missing = Reply {
        status: 404,
        ..reply("text/html", "<p>gone</p>")
    };
    let err = fetch(missing, call).await.unwrap_err();
    assert!(err.contains("HTTP 404"), "{err}");
}

#[tokio::test]
async fn only_http_urls() {
    let client = WebClient::new::<&str>(&[]).unwrap();
    for url in ["file:///etc/passwd", "ftp://example.com/", "not a url"] {
        let err = call(url.into()).run(Some(&client)).await.unwrap_err();
        assert!(err.message.starts_with(url), "{err}");
    }
}

#[tokio::test]
async fn no_network() {
    let err = call("https://example.com/".into())
        .run(None)
        .await
        .unwrap_err();
    assert!(err.message.contains("network"), "{err}");
}

#[tokio::test]
async fn pages_through_long_text() {
    let line = "x".repeat(99) + "\n";
    let body = line.repeat(MAX_FETCH_BYTES / 100 + 10);

    let first = fetch(reply("text/plain", body.clone()), call)
        .await
        .unwrap();
    assert!(first.truncated);
    assert_eq!(first.total_bytes, body.len());
    assert!(first.content.len() <= MAX_FETCH_BYTES);
    assert!(first.content.ends_with('\n'));
    let next = first.next_offset.unwrap();
    assert_eq!(next, first.content.len());

    let rest = fetch(reply("text/plain", body.clone()), |url| FetchTool {
        offset: Some(next),
        ..call(url)
    })
    .await
    .unwrap();
    assert!(!rest.truncated);
    assert_eq!(first.content + &rest.content, body);

    let err = fetch(reply("text/plain", body.clone()), |url| FetchTool {
        offset: Some(body.len() + 1),
        ..call(url)
    })
    .await
    .unwrap_err();
    assert!(err.contains("beyond the end"), "{err}");
}

#[tokio::test]
async fn offset_inside_a_character() {
    let out = fetch(reply("text/plain", "aè"), |url| FetchTool {
        offset: Some(2),
        ..call(url)
    })
    .await
    .unwrap();
    assert_eq!(out.content, "è");
}

#[tokio::test]
async fn huge_line_is_cut() {
    let body = "y".repeat(MAX_FETCH_BYTES + 1);
    let out = fetch(reply("text/plain", body), call).await.unwrap();
    assert_eq!(out.content.len(), MAX_FETCH_BYTES);
    assert_eq!(out.next_offset, Some(MAX_FETCH_BYTES));
}

#[tokio::test]
async fn download_is_capped() {
    let body = vec![b'z'; MAX_FETCH_BODY_BYTES + 1];
    let out = fetch(reply("text/plain", body), call).await.unwrap();
    assert!(out.incomplete);
    assert_eq!(out.total_bytes, MAX_FETCH_BODY_BYTES);
}

#[tokio::test]
async fn offered_only_when_enabled() {
    let model = MockModel::start(vec![text("ok"), text("ok")]).await;

    let mut agent = Agent::with_client(model.client(), "test-model");
    agent.prompt("hi", |_| {}).await.unwrap();
    let client = WebClient::new::<&str>(&[]).unwrap();
    let mut agent = agent.with_web_fetch(client.clone()).with_web_fetch(client);
    agent.prompt("hi", |_| {}).await.unwrap();

    let names = |request: &Value| -> Vec<String> {
        let tools = request["tools"].as_array().unwrap();
        tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let requests = model.requests();
    assert!(!names(&requests[0]).iter().any(|n| n == FETCH_TOOL));
    let count = names(&requests[1])
        .iter()
        .filter(|n| *n == FETCH_TOOL)
        .count();
    assert_eq!(count, 1);
}
