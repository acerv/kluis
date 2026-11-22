// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The `web_fetch` tool: the model reads a web page.
//!
//! It runs on the host, not in the sandbox, so the agent offers it only when
//! the user allows network access.

use std::sync::LazyLock;
use std::time::Duration;

use encoding_rs::Encoding;
use genai::chat::Tool;
use htmd::element_handler::{Element, Handlers};
use htmd::HtmlToMarkdown;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::chain;
use crate::tools::ToolError;

/// Name of the tool.
pub const FETCH_TOOL: &str = "web_fetch";

/// Maximum bytes of page text returned by one call; `offset` pages
/// through the rest.
pub const MAX_FETCH_BYTES: usize = 24 * 1024;

/// Maximum bytes downloaded from a page; the rest is left out.
pub const MAX_FETCH_BODY_BYTES: usize = 5 * 1024 * 1024;

/// Time allowed for a whole request, body included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Tags with no text worth reading: scripts, styling and page chrome.
const SKIPPED_TAGS: [&str; 10] = [
    "script", "style", "noscript", "template", "svg", "img", "iframe", "form", "nav", "footer",
];

/// HTML to Markdown converter, built once.
static MARKDOWN: LazyLock<HtmlToMarkdown> = LazyLock::new(|| {
    HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_TAGS.to_vec())
        .add_handler(vec!["a"], link)
        .build()
});

/// A call of the `web_fetch` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchTool {
    pub url: String,
    /// Return the body as sent instead of HTML converted to Markdown.
    #[serde(default)]
    pub raw: bool,
    /// Byte offset in the page text to start from. Defaults to 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchOutput {
    pub content: String,
    /// Bytes of the whole page text.
    pub total_bytes: usize,
    /// True when more text follows the returned part.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Offset of the text after the returned part, when there is some.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// True when the page was larger than [`MAX_FETCH_BODY_BYTES`] and its
    /// end was not downloaded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

/// What a response body holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Html,
    Text,
}

impl FetchTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "url":    { "type": "string" },
                "raw":    { "type": "boolean" },
                "offset": { "type": "integer" }
            },
            "required": ["url"]
        })
    }

    /// The `web_fetch` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new(FETCH_TOOL)
            .with_description(
                "Fetch a URL. HTML is returned as Markdown unless raw is true. \
                 Use offset to page through long pages.",
            )
            .with_schema(Self::schema())
    }

    /// The HTTP client the tool uses.
    pub fn client() -> Result<Client, ToolError> {
        Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(concat!("kluis/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ToolError::new(chain(&e)))
    }

    /// Fetch the page with `client`, `None` meaning network access is off.
    ///
    /// Only `http` and `https` URLs are allowed. HTML is converted to
    /// Markdown unless `raw`; other text is returned as is, and binary
    /// content is an error. At most [`MAX_FETCH_BYTES`] of text starting at
    /// `offset` are returned, ending at a line break when there is one.
    pub async fn run(self, client: Option<&Client>) -> Result<FetchOutput, ToolError> {
        let client = client.ok_or_else(|| {
            ToolError::new("web_fetch needs network access, which the user has not allowed")
        })?;
        self.fetch(client)
            .await
            .map_err(|e| ToolError::new(format!("{}: {e}", self.url)))
    }

    async fn fetch(&self, client: &Client) -> Result<FetchOutput, String> {
        let url = Url::parse(&self.url).map_err(|e| e.to_string())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("only http and https URLs are allowed".into());
        }
        let mut resp = client.get(url).send().await.map_err(|e| chain(&e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let header = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let (kind, encoding) = parse_content_type(header)
            .ok_or_else(|| format!("unsupported content type {header}"))?;

        // The server's length is a hint, never more than the cap.
        let hint = resp
            .content_length()
            .unwrap_or(0)
            .min(MAX_FETCH_BODY_BYTES as u64);
        let mut body = Vec::with_capacity(hint as usize);
        let mut incomplete = false;
        while let Some(chunk) = resp.chunk().await.map_err(|e| chain(&e))? {
            let room = MAX_FETCH_BODY_BYTES - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                incomplete = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        // Text does not contain NUL bytes; binary content would only reach
        // the model as garbage.
        if memchr::memchr(0, &body).is_some() {
            return Err("binary content".into());
        }

        let markdown = kind == Kind::Html && !self.raw;
        // Parsing megabytes of HTML takes a while: keep it off the runtime.
        let text = tokio::task::spawn_blocking(move || {
            let (text, _, _) = encoding.decode(&body);
            if markdown {
                MARKDOWN.convert(text.as_ref()).map_err(|e| e.to_string())
            } else {
                Ok(text.into_owned())
            }
        })
        .await
        .map_err(|e| e.to_string())??;

        let (content, next_offset) = window(&text, self.offset.unwrap_or(0))?;
        Ok(FetchOutput {
            content: content.to_owned(),
            total_bytes: text.len(),
            truncated: next_offset.is_some(),
            next_offset,
            incomplete,
        })
    }
}

/// The kind of body and its encoding from a `Content-Type` header, or
/// `None` when it is not text. A missing type is taken as text, and a
/// missing or unknown charset as UTF-8.
fn parse_content_type(value: &str) -> Option<(Kind, &'static Encoding)> {
    let mut parts = value.split(';');
    let mime = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let encoding = parts
        .find_map(|p| {
            let (name, label) = p.split_once('=')?;
            if !name.trim().eq_ignore_ascii_case("charset") {
                return None;
            }
            Encoding::for_label(label.trim().trim_matches('"').as_bytes())
        })
        .unwrap_or(encoding_rs::UTF_8);
    let kind = match mime.as_str() {
        "text/html" | "application/xhtml+xml" => Kind::Html,
        "" => Kind::Text,
        m if m.starts_with("text/")
            || m.ends_with("json")
            || m.ends_with("xml")
            || m.ends_with("javascript") =>
        {
            Kind::Text
        }
        _ => return None,
    };
    Some((kind, encoding))
}

/// At most [`MAX_FETCH_BYTES`] of `text` from byte `offset`, cut after the
/// last line break when there is one, and the offset of what follows.
fn window(text: &str, offset: usize) -> Result<(&str, Option<usize>), String> {
    if offset > text.len() {
        return Err(format!(
            "offset {offset} is beyond the end of the page ({} bytes)",
            text.len()
        ));
    }
    let start = floor_char_boundary(text, offset);
    if text.len() - start <= MAX_FETCH_BYTES {
        return Ok((&text[start..], None));
    }
    let mut end = floor_char_boundary(text, start + MAX_FETCH_BYTES);
    if let Some(pos) = text[start..end].rfind('\n') {
        end = start + pos + 1;
    }
    Ok((&text[start..end], Some(end)))
}

/// The largest char boundary of `text` not after `index`.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Links as Markdown, except ones leading nowhere: empty, in-page anchors
/// or scripts, which keep only their text, and links with no text, such as
/// icon buttons, which are dropped.
fn link(
    handlers: &dyn Handlers,
    element: &Element,
) -> Option<htmd::element_handler::HandlerResult> {
    let text = handlers.walk_children(element.node, element.context);
    if text.content.trim().is_empty() {
        return None;
    }
    let href = element.node.attr("href");
    match href.as_deref() {
        Some(href)
            if !href.is_empty() && !href.starts_with('#') && !href.starts_with("javascript:") =>
        {
            handlers.fallback(element)
        }
        _ => Some(text),
    }
}
