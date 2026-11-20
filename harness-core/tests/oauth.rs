// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use common::web::WebServer;
use harness_core::oauth::flow::{self, Flow, FlowConfig, Login};
use harness_core::oauth::http::{BodyFormat, HttpClient};
use oauth2::http::{header::CONTENT_TYPE, Method, Request};
use oauth2::url::Url;
use oauth2::{AsyncHttpClient, TokenResponse};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn token_reply() -> (u16, Value) {
    (
        200,
        json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "token_type": "Bearer",
            "expires_in": 3600,
            "account": {"uuid": "ignored"}
        }),
    )
}

fn config(server: &WebServer, flow: Flow) -> FlowConfig {
    FlowConfig {
        client_id: "cid".into(),
        token_url: server.url("/token"),
        scopes: vec!["a".into(), "b".into()],
        auth_params: vec![("code".into(), "true".into())],
        body: BodyFormat::Json,
        flow,
    }
}

fn query(url: &str, name: &str) -> Option<String> {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// Send a GET for `target` to the local callback server and return the reply.
async fn browse(port: u16, target: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!("GET {target} HTTP/1.1\r\nhost: localhost\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.unwrap();
    resp
}

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
async fn paste_login_builds_pkce_url_and_exchanges_code() {
    let server = WebServer::start(vec![token_reply()]).await;
    let flow = Flow::Paste {
        auth_url: "https://auth.example/authorize".into(),
        redirect_uri: "https://auth.example/callback".into(),
    };
    let Login::Paste(login) = flow::start(&config(&server, flow)).await.unwrap() else {
        panic!("expected a paste login");
    };
    let url = login.url().to_owned();
    assert!(url.starts_with("https://auth.example/authorize?"), "{url}");
    assert_eq!(query(&url, "client_id").as_deref(), Some("cid"));
    assert_eq!(query(&url, "response_type").as_deref(), Some("code"));
    assert_eq!(query(&url, "scope").as_deref(), Some("a b"));
    assert_eq!(query(&url, "code").as_deref(), Some("true"));
    assert_eq!(
        query(&url, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(query(&url, "code_challenge").is_some());
    let state = query(&url, "state").unwrap();

    let token = login.finish(&format!(" the-code#{state}\n")).await.unwrap();
    assert_eq!(token.access_token().secret(), "at-1");
    assert_eq!(token.refresh_token().unwrap().secret(), "rt-1");

    let body = server.requests()[0].json();
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "the-code");
    assert_eq!(body["state"], state.as_str());
    assert_eq!(body["client_id"], "cid");
    assert_eq!(body["redirect_uri"], "https://auth.example/callback");
    assert!(body["code_verifier"].as_str().unwrap().len() >= 43);
}

#[tokio::test]
async fn paste_login_rejects_state_mismatch() {
    let server = WebServer::start(vec![token_reply()]).await;
    let flow = Flow::Paste {
        auth_url: "https://auth.example/authorize".into(),
        redirect_uri: "https://auth.example/callback".into(),
    };
    let Login::Paste(login) = flow::start(&config(&server, flow)).await.unwrap() else {
        panic!("expected a paste login");
    };
    let err = login.finish("code#forged").await.unwrap_err().to_string();
    assert!(err.contains("state mismatch"), "{err}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn callback_login_receives_browser_redirect() {
    let server = WebServer::start(vec![token_reply()]).await;
    let flow = Flow::Callback {
        auth_url: "https://auth.example/authorize".into(),
        port: 0,
        path: "/cb".into(),
    };
    let Login::Callback(login) = flow::start(&config(&server, flow)).await.unwrap() else {
        panic!("expected a callback login");
    };
    let port = login.port();
    let redirect = format!("http://localhost:{port}/cb");
    assert_eq!(query(login.url(), "redirect_uri"), Some(redirect.clone()));
    let state = query(login.url(), "state").unwrap();

    let browser = tokio::spawn(async move {
        let other = browse(port, "/favicon.ico").await;
        let done = browse(port, &format!("/cb?code=c-1&state={state}")).await;
        (other, done)
    });
    let token = login.finish().await.unwrap();
    assert_eq!(token.access_token().secret(), "at-1");

    let (other, done) = browser.await.unwrap();
    assert!(other.starts_with("HTTP/1.1 404"), "{other}");
    assert!(done.starts_with("HTTP/1.1 200"), "{done}");
    let body = server.requests()[0].json();
    assert_eq!(body["code"], "c-1");
    assert_eq!(body["redirect_uri"], redirect.as_str());
}

#[tokio::test]
async fn callback_login_reports_provider_error() {
    let server = WebServer::start(vec![]).await;
    let flow = Flow::Callback {
        auth_url: "https://auth.example/authorize".into(),
        port: 0,
        path: "/cb".into(),
    };
    let Login::Callback(login) = flow::start(&config(&server, flow)).await.unwrap() else {
        panic!("expected a callback login");
    };
    let port = login.port();
    let browser = tokio::spawn(async move { browse(port, "/cb?error=access_denied").await });
    let err = login.finish().await.unwrap_err().to_string();
    assert!(err.contains("access_denied"), "{err}");
    assert!(browser.await.unwrap().starts_with("HTTP/1.1 400"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn device_login_polls_for_token() {
    let server = WebServer::start(vec![
        (
            200,
            json!({
                "device_code": "dc",
                "user_code": "ABCD-1234",
                "verification_uri": "https://auth.example/device",
                "expires_in": 900,
                "interval": 1
            }),
        ),
        token_reply(),
    ])
    .await;
    let flow = Flow::Device {
        device_url: server.url("/device"),
    };
    let mut cfg = config(&server, flow);
    cfg.body = BodyFormat::Form;
    let Login::Device(login) = flow::start(&cfg).await.unwrap() else {
        panic!("expected a device login");
    };
    assert_eq!(login.url(), "https://auth.example/device");
    assert_eq!(login.user_code(), "ABCD-1234");
    let token = login.finish().await.unwrap();
    assert_eq!(token.access_token().secret(), "at-1");

    let reqs = server.requests();
    assert_eq!(reqs[0].path, "/device");
    assert!(reqs[0].body.contains("scope=a+b"), "{}", reqs[0].body);
    assert_eq!(reqs[1].path, "/token");
    assert!(reqs[1].body.contains("device_code=dc"), "{}", reqs[1].body);
}

#[tokio::test]
async fn refresh_sends_refresh_token_and_reports_errors() {
    let server = WebServer::start(vec![
        token_reply(),
        (400, json!({"error": "invalid_grant"})),
    ])
    .await;
    let cfg = config(
        &server,
        Flow::Device {
            device_url: server.url("/device"),
        },
    );
    let token = flow::refresh(&cfg, "rt-0").await.unwrap();
    assert_eq!(token.access_token().secret(), "at-1");
    let body = server.requests()[0].json();
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], "rt-0");

    let err = flow::refresh(&cfg, "rt-0").await.unwrap_err().to_string();
    assert!(err.contains("invalid_grant"), "{err}");
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
