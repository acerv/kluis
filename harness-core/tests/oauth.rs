// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use common::web::WebServer;
use harness_core::oauth::http::{BodyFormat, HttpClient};
use oauth2::http::{header::CONTENT_TYPE, Method, Request};
use oauth2::AsyncHttpClient;
use serde_json::json;

fn form_request(url: &str) -> Request<Vec<u8>> {
    Request::builder()
        .method(Method::POST)
        .uri(url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(b"grant_type=refresh_token&scope=a+b&x=%26".to_vec())
        .unwrap()
}

#[tokio::test]
async fn http_form_body_is_sent_unchanged() {
    let server = WebServer::start(vec![(200, json!({"ok": true}))]).await;
    let http = HttpClient::new(BodyFormat::Form).unwrap();
    let resp = http
        .call(form_request(&server.url("/token")))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), br#"{"ok":true}"#);

    let req = &server.requests()[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/token");
    assert_eq!(
        req.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(req.body, "grant_type=refresh_token&scope=a+b&x=%26");
}

#[tokio::test]
async fn http_json_body_converts_form_fields() {
    let server = WebServer::start(vec![(400, json!({"error": "invalid_grant"}))]).await;
    let http = HttpClient::new(BodyFormat::Json).unwrap();
    let resp = http
        .call(form_request(&server.url("/token")))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let req = &server.requests()[0];
    assert_eq!(req.header("content-type"), Some("application/json"));
    assert_eq!(
        req.json(),
        json!({"grant_type": "refresh_token", "scope": "a b", "x": "&"})
    );
}
