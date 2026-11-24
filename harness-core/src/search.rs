// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The `web_search` tool: the model searches the web.
//!
//! Like `web_fetch`, it runs on the host, so the agent offers it only when
//! the user allows network access. It reads the results page of
//! DuckDuckGo's HTML search, which needs no account, or asks the JSON API
//! of a SearXNG instance.

use genai::chat::Tool;
use htmd::node::Node;
use htmd::HtmlToMarkdown;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::chain;
use crate::fetch::{is_web, read_body, FetchTool};
use crate::tools::ToolError;

/// Name of the tool.
pub const SEARCH_TOOL: &str = "web_search";

/// Search page asked by default.
pub const DUCKDUCKGO_URL: &str = "https://html.duckduckgo.com/html/";

/// Maximum results returned by one call.
pub const MAX_SEARCH_RESULTS: usize = 10;

/// Maximum bytes downloaded from a results page; the rest is left out.
const MAX_PAGE_BYTES: usize = 1024 * 1024;

/// A call of the `web_search` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchTool {
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snippet: String,
}

/// The kind of search engine asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// DuckDuckGo's HTML results page.
    DuckDuckGo,
    /// The JSON API of a SearXNG instance.
    Searxng,
}

/// Where and how the tool searches.
#[derive(Debug, Clone)]
pub struct Searcher {
    client: Client,
    engine: Engine,
    endpoint: Url,
}

impl Searcher {
    /// A searcher asking `engine` at `endpoint`: the results page for
    /// DuckDuckGo, the instance URL for SearXNG.
    pub fn new(engine: Engine, endpoint: &str) -> Result<Self, ToolError> {
        let mut endpoint = Url::parse(endpoint)
            .map_err(|e| ToolError::new(format!("search URL {endpoint}: {e}")))?;
        if engine == Engine::Searxng {
            // The API is the instance's /search, wherever it is mounted.
            if !endpoint.path().ends_with('/') {
                endpoint.set_path(&format!("{}/", endpoint.path()));
            }
            endpoint = endpoint
                .join("search")
                .map_err(|e| ToolError::new(e.to_string()))?;
        }
        Ok(Self {
            client: FetchTool::client()?,
            engine,
            endpoint,
        })
    }

    /// A searcher asking DuckDuckGo.
    pub fn duckduckgo() -> Result<Self, ToolError> {
        Self::new(Engine::DuckDuckGo, DUCKDUCKGO_URL)
    }
}

impl SearchTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" }
            },
            "required": ["query"]
        })
    }

    /// The `web_search` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new(SEARCH_TOOL)
            .with_description(
                "Search the web. Returns titles, URLs and snippets; \
                 read a page with web_fetch.",
            )
            .with_schema(Self::schema())
    }

    /// Search with `searcher`, `None` meaning network access is off.
    pub async fn run(self, searcher: Option<&Searcher>) -> Result<SearchOutput, ToolError> {
        let searcher = searcher.ok_or_else(|| {
            ToolError::new("web_search needs network access, which the user has not allowed")
        })?;
        if self.query.trim().is_empty() {
            return Err(ToolError::new("query is empty"));
        }
        self.search(searcher)
            .await
            .map_err(|e| ToolError::new(format!("search failed: {e}")))
    }

    async fn search(&self, searcher: &Searcher) -> Result<SearchOutput, String> {
        let mut url = searcher.endpoint.clone();
        url.query_pairs_mut().append_pair("q", &self.query);
        if searcher.engine == Engine::Searxng {
            url.query_pairs_mut().append_pair("format", "json");
        }
        let resp = searcher
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| chain(&e))?;
        // DuckDuckGo answers 202 with a challenge when it takes the client
        // for a bot: only 200 carries results.
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            return Err(format!("HTTP {status}"));
        }
        let (body, _) = read_body(resp, MAX_PAGE_BYTES).await?;
        match searcher.engine {
            Engine::Searxng => parse_searxng(&body),
            Engine::DuckDuckGo => {
                let base = searcher.endpoint.clone();
                // Parsing HTML takes a while: keep it off the runtime.
                tokio::task::spawn_blocking(move || {
                    parse_duckduckgo(&String::from_utf8_lossy(&body), &base)
                })
                .await
                .map_err(|e| e.to_string())?
            }
        }
    }
}

/// The results of a SearXNG JSON answer. Only http and https links are
/// kept.
fn parse_searxng(body: &[u8]) -> Result<SearchOutput, String> {
    #[derive(Deserialize)]
    struct Answer {
        results: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        title: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        content: String,
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let results = answer
        .results
        .into_iter()
        .filter(|item| Url::parse(&item.url).is_ok_and(|u| is_web(&u)))
        .take(MAX_SEARCH_RESULTS)
        .map(|item| SearchResult {
            title: clean(&item.title),
            url: item.url,
            snippet: clean(&item.content),
        })
        .collect();
    Ok(SearchOutput { results })
}

/// The results listed in a DuckDuckGo HTML results page, links resolved
/// against `base`. Ads are left out.
fn parse_duckduckgo(html: &str, base: &Url) -> Result<SearchOutput, String> {
    let tree = HtmlToMarkdown::new()
        .html_to_tree(html)
        .map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    let mut challenged = false;
    walk(&tree.root(), |node| {
        if has_class(node, "anomaly-modal__modal") {
            challenged = true;
        } else if results.len() < MAX_SEARCH_RESULTS
            && has_class(node, "result")
            && !has_class(node, "result--ad")
        {
            results.extend(parse_result(node, base));
        }
    });
    if results.is_empty() && challenged {
        return Err("the search engine asked to prove a human is searching".into());
    }
    Ok(SearchOutput { results })
}

/// One result, or `None` when it has no title or no web link.
fn parse_result(node: &Node, base: &Url) -> Option<SearchResult> {
    let link = node.find(|n| has_class(n, "result__a"))?;
    let url = target(&link.attr("href")?, base)?;
    let title = text(&link);
    if title.is_empty() {
        return None;
    }
    let snippet = node
        .find(|n| has_class(n, "result__snippet"))
        .map(|n| text(&n))
        .unwrap_or_default();
    Some(SearchResult {
        title,
        url,
        snippet,
    })
}

/// Where `href` leads: DuckDuckGo wraps links in a redirect carrying the
/// target in `uddg`. Only http and https targets are kept.
fn target(href: &str, base: &Url) -> Option<String> {
    let mut url = base.join(href).ok()?;
    if url.path() == "/l/" {
        let (_, inner) = url.query_pairs().find(|(k, _)| k == "uddg")?;
        url = Url::parse(&inner).ok()?;
    }
    is_web(&url).then(|| url.into())
}

/// Call `f` on `node` and its descendants.
fn walk(node: &Node, mut f: impl FnMut(&Node)) {
    // A search visiting every node: no recursion, so no deep page can
    // overflow the stack.
    node.find(|n| {
        f(n);
        false
    });
}

/// Whether `node` is an element with `class` among its classes.
fn has_class(node: &Node, class: &str) -> bool {
    node.attr("class")
        .is_some_and(|c| c.split_ascii_whitespace().any(|c| c == class))
}

/// The text of `node`, with runs of white space made one space.
fn text(node: &Node) -> String {
    let mut raw = String::new();
    walk(node, |n| {
        if let Some(t) = n.text() {
            raw.push_str(&t);
        }
    });
    clean(&raw)
}

/// `text` with runs of white space made one space.
fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
