// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use common::web::{Reply, WebServer};
use common::{text, MockModel};
use harness_core::tools::{
    Engine, SearchOutput, SearchResult, SearchTool, Searcher, MAX_SEARCH_RESULTS, SEARCH_TOOL,
};
use harness_core::Agent;
use serde_json::{json, Value};

fn reply(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status,
        content_type,
        body: body.into(),
    }
}

/// Serve `reply` once, search `query` with `engine` at `path` of the
/// server, and return the outcome with the path asked.
async fn search(
    engine: Engine,
    path: &str,
    reply: Reply,
    query: &str,
) -> (Result<SearchOutput, String>, String) {
    let server = WebServer::serve(vec![reply]).await;
    let searcher = Searcher::new(engine, &server.url(path)).unwrap();
    let out = SearchTool {
        query: query.to_owned(),
    }
    .run(Some(&searcher))
    .await
    .map_err(|e| e.message);
    let asked = server
        .requests()
        .first()
        .map(|r| r.path.clone())
        .unwrap_or_default();
    (out, asked)
}

async fn duckduckgo(reply: Reply) -> Result<SearchOutput, String> {
    search(Engine::DuckDuckGo, "/html/", reply, "rust sandbox")
        .await
        .0
}

fn result(title: &str, url: &str, snippet: &str) -> SearchResult {
    SearchResult {
        title: title.to_owned(),
        url: url.to_owned(),
        snippet: snippet.to_owned(),
    }
}

const PAGE: &str = r#"<html><body>
<div class="result results_links result--ad">
  <h2 class="result__title"><a class="result__a" href="https://duckduckgo.com/y.js?ad=1">Ad</a></h2>
  <a class="result__snippet" href="https://duckduckgo.com/y.js?ad=1">Buy now</a>
</div>
<div class="result results_links results_links_deep web-result ">
  <div class="links_main links_deep result__body">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1&amp;rut=abc">The <b>Rust</b>
        sandbox</a>
    </h2>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa">A <b>sandbox</b> for
      Rust.</a>
  </div>
</div>
<div class="result web-result">
  <h2 class="result__title"><a class="result__a" href="https://example.org/b">Direct</a></h2>
</div>
<div class="result web-result">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=javascript%3Aalert(1)">Script</a></h2>
</div>
<div class="result web-result">
  <h2 class="result__title"><a class="result__a" href="https://example.net/"></a></h2>
</div>
</body></html>"#;

#[test]
fn definition() {
    let tool = SearchTool::definition();
    assert_eq!(tool.name.as_ref(), SEARCH_TOOL);
    assert_eq!(tool.schema, Some(SearchTool::schema()));
}

#[tokio::test]
async fn duckduckgo_results() {
    let (out, asked) = search(
        Engine::DuckDuckGo,
        "/html/",
        reply(200, "text/html", PAGE),
        "rust sandbox",
    )
    .await;
    assert_eq!(asked, "/html/?q=rust+sandbox");
    assert_eq!(
        out.unwrap().results,
        vec![
            result(
                "The Rust sandbox",
                "https://example.com/a?x=1",
                "A sandbox for Rust."
            ),
            result("Direct", "https://example.org/b", ""),
        ]
    );
}

#[tokio::test]
async fn duckduckgo_keeps_at_most_max_results() {
    let one = r#"<div class="result"><a class="result__a" href="https://example.com/">R</a></div>"#;
    let page = one.repeat(MAX_SEARCH_RESULTS + 5);
    let out = duckduckgo(reply(200, "text/html", page)).await.unwrap();
    assert_eq!(out.results.len(), MAX_SEARCH_RESULTS);
}

#[tokio::test]
async fn duckduckgo_no_results() {
    let out = duckduckgo(reply(200, "text/html", "<p>No results.</p>"))
        .await
        .unwrap();
    assert!(out.results.is_empty());
}

#[tokio::test]
async fn duckduckgo_challenge_is_an_error() {
    let page = r#"<div class="anomaly-modal__modal">Select all squares</div>"#;
    let err = duckduckgo(reply(200, "text/html", page)).await.unwrap_err();
    assert!(err.contains("human"), "{err}");

    // The challenge also comes with 202.
    let err = duckduckgo(reply(202, "text/html", page)).await.unwrap_err();
    assert!(err.contains("202"), "{err}");
}

#[tokio::test]
async fn searxng_results() {
    let body = json!({
        "query": "rust",
        "results": [
            { "title": "Rust", "url": "https://www.rust-lang.org/", "content": "A  language\nfor all" },
            { "title": "No snippet", "url": "http://example.com/" },
            { "title": "Local", "url": "file:///etc/passwd", "content": "x" },
            { "title": "No URL" }
        ]
    });
    let (out, asked) = search(
        Engine::Searxng,
        "/searx",
        reply(200, "application/json", body.to_string()),
        "rust",
    )
    .await;
    assert_eq!(asked, "/searx/search?q=rust&format=json");
    assert_eq!(
        out.unwrap().results,
        vec![
            result("Rust", "https://www.rust-lang.org/", "A language for all"),
            result("No snippet", "http://example.com/", ""),
        ]
    );
}

#[tokio::test]
async fn searxng_at_the_root() {
    let body = json!({ "results": [] }).to_string();
    let (out, asked) = search(
        Engine::Searxng,
        "/",
        reply(200, "application/json", body),
        "a&b",
    )
    .await;
    assert!(out.unwrap().results.is_empty());
    assert_eq!(asked, "/search?q=a%26b&format=json");
}

#[tokio::test]
async fn searxng_errors() {
    // Instances with the JSON format turned off answer 403.
    let (out, _) = search(
        Engine::Searxng,
        "/",
        reply(403, "text/html", "Forbidden"),
        "x",
    )
    .await;
    assert!(out.unwrap_err().contains("403"));

    let (out, _) = search(Engine::Searxng, "/", reply(200, "text/html", "<html>"), "x").await;
    assert!(out.unwrap_err().starts_with("search failed"));
}

#[tokio::test]
async fn refused_without_network_or_query() {
    let tool = |query: &str| SearchTool {
        query: query.to_owned(),
    };
    let err = tool("x").run(None).await.unwrap_err();
    assert!(err.message.contains("network access"));

    let searcher = Searcher::duckduckgo().unwrap();
    let err = tool("  ").run(Some(&searcher)).await.unwrap_err();
    assert_eq!(err.message, "query is empty");
}

#[test]
fn invalid_endpoint() {
    let err = Searcher::new(Engine::Searxng, "not a url").unwrap_err();
    assert!(err.message.contains("not a url"));
}

#[tokio::test]
async fn offered_only_when_enabled() {
    let model = MockModel::start(vec![text("ok"), text("ok")]).await;

    let mut agent = Agent::with_client(model.client(), "test-model");
    agent.prompt("hi", |_| {}).await.unwrap();
    let searcher = Searcher::duckduckgo().unwrap();
    let mut agent = agent
        .with_web_search(searcher.clone())
        .with_web_search(searcher);
    agent.prompt("hi", |_| {}).await.unwrap();

    let names = |request: &Value| -> Vec<String> {
        let tools = request["tools"].as_array().unwrap();
        tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let requests = model.requests();
    assert!(!names(&requests[0]).iter().any(|n| n == SEARCH_TOOL));
    let count = names(&requests[1])
        .iter()
        .filter(|n| *n == SEARCH_TOOL)
        .count();
    assert_eq!(count, 1);
}
