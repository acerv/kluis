// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::time::Duration;

use common::{client_for, text, MockModel, Reply};
use genai::adapter::AdapterKind;
use genai::chat::ChatRole;
use harness_core::{Agent, Event, RetryPolicy};

/// Retries a test can wait for: a few, and quick.
const FAST: RetryPolicy = RetryPolicy {
    max_retries: 2,
    base_delay: Duration::from_millis(1),
    max_delay: Duration::from_secs(5),
};

fn status(code: u16) -> Reply {
    Reply::Status(code, Vec::new(), "{}".into())
}

fn status_with(code: u16, header: &'static str, value: &str) -> Reply {
    Reply::Status(code, vec![(header, value.to_owned())], "{}".into())
}

/// The retries announced while running a prompt, as (attempt, delay,
/// reason), and its outcome.
async fn run(
    agent: &mut Agent,
) -> (
    Vec<(u32, Duration, String)>,
    Result<String, harness_core::Error>,
) {
    let mut retries = Vec::new();
    let run = agent.prompt("hi", |event| {
        if let Event::Retry {
            attempt,
            delay,
            reason,
        } = event
        {
            retries.push((attempt, delay, reason.to_owned()));
        }
    });
    let out = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("prompt hangs");
    (retries, out)
}

async fn setup(replies: Vec<Reply>, policy: RetryPolicy) -> (MockModel, Agent) {
    let model = MockModel::start(replies).await;
    let agent = Agent::with_client(model.client(), "test-model").with_retry(policy);
    (model, agent)
}

#[test]
fn default_policy() {
    let policy = RetryPolicy::default();
    assert_eq!(policy.max_retries, 5);
    assert_eq!(policy.base_delay, Duration::from_secs(1));
    assert_eq!(policy.max_delay, Duration::from_secs(60));
}

#[tokio::test]
async fn rate_limit_is_retried() {
    let (model, mut agent) = setup(vec![status(429), text("ok")], FAST).await;

    let (retries, out) = run(&mut agent).await;
    assert_eq!(out.unwrap(), "ok");
    assert_eq!(
        retries,
        [(
            1,
            Duration::from_millis(1),
            "HTTP 429 Too Many Requests".into()
        )]
    );
    assert_eq!(model.requests().len(), 2);
    // The failed attempt leaves nothing in the history.
    let roles: Vec<_> = agent.history().iter().map(|m| m.role.clone()).collect();
    assert_eq!(roles, [ChatRole::User, ChatRole::Assistant]);
}

#[tokio::test]
async fn backoff_doubles_then_gives_up() {
    let (model, mut agent) = setup(vec![status(500), status(502), status(503)], FAST).await;

    let (retries, out) = run(&mut agent).await;
    assert!(out.unwrap_err().to_string().contains("503"));
    let delays: Vec<_> = retries.iter().map(|(n, d, _)| (*n, *d)).collect();
    assert_eq!(
        delays,
        [(1, Duration::from_millis(1)), (2, Duration::from_millis(2))]
    );
    assert_eq!(model.requests().len(), 3);
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    for code in [400, 401, 403, 404] {
        let (model, mut agent) = setup(vec![status(code)], FAST).await;
        let (retries, out) = run(&mut agent).await;
        assert!(out.is_err(), "{code}");
        assert!(retries.is_empty(), "{code}");
        assert_eq!(model.requests().len(), 1, "{code}");
    }
}

#[tokio::test]
async fn retry_after_is_honored() {
    let slow = RetryPolicy {
        base_delay: Duration::from_secs(30),
        ..FAST
    };
    let replies = vec![
        status_with(503, "retry-after", "0"),
        status_with(429, "retry-after-ms", "5"),
        text("ok"),
    ];
    let (_model, mut agent) = setup(replies, slow).await;

    let (retries, out) = run(&mut agent).await;
    assert_eq!(out.unwrap(), "ok");
    let delays: Vec<_> = retries.iter().map(|(_, d, _)| *d).collect();
    assert_eq!(delays, [Duration::ZERO, Duration::from_millis(5)]);
}

#[tokio::test]
async fn bad_retry_after_falls_back_to_backoff() {
    let replies = vec![
        status_with(503, "retry-after-ms", "inf"),
        status_with(503, "retry-after-ms", "-5"),
        status_with(503, "retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"),
        text("ok"),
    ];
    let policy = RetryPolicy {
        max_retries: 3,
        ..FAST
    };
    let (_model, mut agent) = setup(replies, policy).await;

    let (retries, out) = run(&mut agent).await;
    assert_eq!(out.unwrap(), "ok");
    let delays: Vec<_> = retries.iter().map(|(_, d, _)| d.as_millis()).collect();
    assert_eq!(delays, [1, 2, 4]);
}

#[tokio::test]
async fn long_retry_after_is_not_waited() {
    let (model, mut agent) = setup(vec![status_with(429, "retry-after", "3600")], FAST).await;

    let (retries, out) = run(&mut agent).await;
    assert!(out.unwrap_err().to_string().contains("429"));
    assert!(retries.is_empty());
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test]
async fn should_retry_header_wins() {
    let replies = vec![status_with(503, "x-should-retry", "false")];
    let (model, mut agent) = setup(replies, FAST).await;
    let (retries, _) = run(&mut agent).await;
    assert!(retries.is_empty());
    assert_eq!(model.requests().len(), 1);

    let replies = vec![status_with(400, "x-should-retry", "true"), text("ok")];
    let (_model, mut agent) = setup(replies, FAST).await;
    let (retries, out) = run(&mut agent).await;
    assert_eq!(out.unwrap(), "ok");
    assert_eq!(retries.len(), 1);
}

#[tokio::test]
async fn dropped_stream_is_retried() {
    let replies = vec![Reply::Broken("half an ans".into()), text("whole answer")];
    let (_model, mut agent) = setup(replies, FAST).await;

    let (retries, out) = run(&mut agent).await;
    assert_eq!(out.unwrap(), "whole answer");
    assert_eq!(retries[0].2, "connection lost");
}

#[tokio::test]
async fn unreachable_server_is_not_retried() {
    // Bind a port, then free it, so nothing listens there.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let client = client_for(format!("http://127.0.0.1:{port}/v1/"), AdapterKind::OpenAI);
    let mut agent = Agent::with_client(client, "test-model").with_retry(FAST);

    // No network or a server down: a retry would fail the same way.
    let (retries, out) = run(&mut agent).await;
    assert!(out.is_err());
    assert!(retries.is_empty());
}

#[tokio::test]
async fn no_retries() {
    let off = RetryPolicy {
        max_retries: 0,
        ..FAST
    };
    let (model, mut agent) = setup(vec![status(503)], off).await;

    let (retries, out) = run(&mut agent).await;
    assert!(out.is_err());
    assert!(retries.is_empty());
    assert_eq!(model.requests().len(), 1);
}
