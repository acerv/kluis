// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A fake OpenAI-compatible model server for agent tests. It streams its
//! replies as server-sent events, as the agent always asks for a stream.

#![allow(dead_code)]

pub mod web;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Barrier;

/// Characters per streamed piece of text.
const PIECE: usize = 3;

/// Token usage reported at the end of every complete reply.
pub const USAGE: (i32, i32) = (10, 5);

/// What the server answers to a request.
pub enum Reply {
    /// A whole model turn: reasoning, text, then tool calls, then usage.
    Turn {
        reasoning: String,
        text: String,
        calls: Vec<Value>,
        finish_reason: Option<String>,
        usage: (i32, i32),
    },
    /// The first pieces of some text, then nothing more.
    Partial(String),
    /// The first pieces of some text, then the connection closes.
    Broken(String),
    /// No answer at all, as a model that takes forever.
    Hang,
    /// An HTTP error: status, extra headers and body.
    Status(u16, Vec<(&'static str, String)>, String),
    /// A reply sent only once all the requests sharing the barrier came.
    Gated(Arc<Barrier>, Box<Reply>),
}

/// `replies`, each sent only once all of them have been asked for: the
/// requests getting them must be made at the same time.
pub fn together(replies: Vec<Reply>) -> Vec<Reply> {
    let barrier = Arc::new(Barrier::new(replies.len()));
    replies
        .into_iter()
        .map(|reply| Reply::Gated(barrier.clone(), Box::new(reply)))
        .collect()
}

/// A reply with only text.
pub fn text(text: &str) -> Reply {
    Reply::Turn {
        reasoning: String::new(),
        text: text.into(),
        calls: Vec::new(),
        finish_reason: None,
        usage: USAGE,
    }
}

/// `reply`, with custom `(prompt, completion)` token usage.
pub fn with_usage(prompt: i32, completion: i32, reply: Reply) -> Reply {
    match reply {
        Reply::Turn {
            reasoning,
            text,
            calls,
            finish_reason,
            ..
        } => Reply::Turn {
            reasoning,
            text,
            calls,
            finish_reason,
            usage: (prompt, completion),
        },
        _ => panic!("only a whole turn has usage"),
    }
}

/// `reply`, with the model reasoning first.
pub fn thinking(reasoning: &str, reply: Reply) -> Reply {
    match reply {
        Reply::Turn {
            text,
            calls,
            finish_reason,
            usage,
            ..
        } => Reply::Turn {
            reasoning: reasoning.into(),
            text,
            calls,
            finish_reason,
            usage,
        },
        _ => panic!("only a whole turn has reasoning"),
    }
}

/// A reply calling tools, given as `(id, name, arguments)`.
pub fn tool_calls(calls: &[(&str, &str, Value)]) -> Reply {
    let raw: Vec<_> = calls
        .iter()
        .map(|(id, name, args)| (*id, *name, args.to_string()))
        .collect();
    tool_calls_turn(&raw, None)
}

/// A truncated (`finish_reason = "length"`) reply with raw argument strings,
/// allowing incomplete JSON to be streamed just like a `max_tokens` cut.
pub fn truncated_tool_calls(calls: &[(&str, &str, &str)]) -> Reply {
    truncated(raw_tool_calls(calls))
}

/// A reply calling tools with raw argument strings, as a model writing
/// arguments that are not JSON does.
pub fn raw_tool_calls(calls: &[(&str, &str, &str)]) -> Reply {
    let raw: Vec<_> = calls
        .iter()
        .map(|(id, name, args)| (*id, *name, (*args).to_owned()))
        .collect();
    tool_calls_turn(&raw, None)
}

/// `reply`, cut at the token limit (`finish_reason = "length"`).
pub fn truncated(reply: Reply) -> Reply {
    match reply {
        Reply::Turn {
            reasoning,
            text,
            calls,
            usage,
            ..
        } => Reply::Turn {
            reasoning,
            text,
            calls,
            finish_reason: Some("length".into()),
            usage,
        },
        _ => panic!("only a whole turn can be cut"),
    }
}

fn tool_calls_turn(calls: &[(&str, &str, String)], finish_reason: Option<String>) -> Reply {
    let calls = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args }
            })
        })
        .collect();
    Reply::Turn {
        reasoning: String::new(),
        text: String::new(),
        calls,
        finish_reason,
        usage: USAGE,
    }
}

/// Wire protocol spoken by [`MockModel`].
#[derive(Clone, Copy)]
enum Wire {
    OpenAi,
    Anthropic,
}

/// A server answering chat requests with scripted replies, in order.
pub struct MockModel {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockModel {
    pub async fn start(replies: Vec<Reply>) -> Self {
        Self::start_with(replies, Wire::OpenAi).await
    }

    pub async fn start_anthropic(replies: Vec<Reply>) -> Self {
        Self::start_with(replies, Wire::Anthropic).await
    }

    async fn start_with(replies: Vec<Reply>, wire: Wire) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));

        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(stream, seen.clone(), replies.clone(), wire));
            }
        });
        Self { url, requests }
    }

    /// A client sending every request to this server.
    pub fn client(&self) -> Client {
        self.client_as(AdapterKind::OpenAI)
    }

    /// A client sending every request to this server, through the adapter
    /// of `kind`, as when talking to that provider.
    pub fn client_as(&self, kind: AdapterKind) -> Client {
        client_for(self.url.clone(), kind)
    }

    /// Bodies of the requests received so far.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// A client sending every request to `url`, through the adapter of `kind`.
pub fn client_for(url: String, kind: AdapterKind) -> Client {
    let resolver = ServiceTargetResolver::from_resolver_fn(
        move |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            Ok(ServiceTarget {
                endpoint: Endpoint::from_owned(url.clone()),
                auth: AuthData::from_single("test"),
                model: ModelIden::new(kind, target.model.model_name),
            })
        },
    );
    Client::builder()
        .with_service_target_resolver(resolver)
        .build()
        .unwrap()
}

/// Answer one request, then close the connection.
async fn serve(
    stream: TcpStream,
    seen: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    wire: Wire,
) {
    let mut stream = BufReader::new(stream);
    let mut length = 0;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    seen.lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap());

    let reply = replies.lock().unwrap().pop_front();
    let mut reply = reply.expect("no reply left for this request");
    if let Reply::Gated(barrier, inner) = reply {
        barrier.wait().await;
        reply = *inner;
    }
    let out = stream.get_mut();
    match reply {
        Reply::Hang => return std::future::pending().await,
        Reply::Status(status, headers, body) => {
            let mut head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            out.write_all(head.as_bytes()).await.unwrap();
            out.write_all(body.as_bytes()).await.unwrap();
            return;
        }
        _ => {}
    }
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    out.write_all(head.as_bytes()).await.unwrap();
    match (wire, reply) {
        (
            Wire::Anthropic,
            Reply::Turn {
                text,
                calls,
                finish_reason,
                usage,
                ..
            },
        ) => {
            serve_anthropic_turn(out, &text, &calls, finish_reason.as_deref(), usage).await;
        }
        (
            Wire::OpenAi,
            Reply::Turn {
                reasoning,
                text,
                calls,
                finish_reason,
                usage: (prompt, completion),
            },
        ) => {
            for piece in pieces(&reasoning) {
                send(out, delta(json!({ "reasoning_content": piece }), None)).await;
            }
            for piece in pieces(&text) {
                send(out, delta(json!({ "content": piece }), None)).await;
            }
            for call in &calls {
                send(out, delta(json!({ "tool_calls": [call] }), None)).await;
            }
            let reason = finish_reason.as_deref().unwrap_or(if calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            });
            send(out, delta(json!({}), Some(reason))).await;
            let usage = json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "total_tokens": prompt + completion
            });
            send(out, json!({ "choices": [], "usage": usage }).to_string()).await;
            send(out, "[DONE]".into()).await;
        }
        (_, Reply::Partial(text)) => {
            for piece in pieces(&text) {
                send(out, delta(json!({ "content": piece }), None)).await;
            }
            std::future::pending::<()>().await;
        }
        // Returning drops the connection in the middle of the reply.
        (_, Reply::Broken(text)) => {
            for piece in pieces(&text) {
                send(out, delta(json!({ "content": piece }), None)).await;
            }
        }
        (_, Reply::Hang | Reply::Status(..) | Reply::Gated(..)) => unreachable!(),
    }
}

async fn serve_anthropic_turn(
    out: &mut TcpStream,
    text: &str,
    calls: &[Value],
    finish_reason: Option<&str>,
    (prompt, completion): (i32, i32),
) {
    send_event(
        out,
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": "claude-sonnet-4-5",
                "stop_reason": null,
                "usage": { "input_tokens": prompt, "output_tokens": 0 }
            }
        }),
    )
    .await;

    let mut index = 0usize;
    if !text.is_empty() {
        send_event(
            out,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "text", "text": "" }
            }),
        )
        .await;
        for piece in pieces(text) {
            send_event(
                out,
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "text_delta", "text": piece }
                }),
            )
            .await;
        }
        send_event(
            out,
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": index }),
        )
        .await;
        index += 1;
    }

    for call in calls {
        let id = call["id"].as_str().unwrap();
        let name = call["function"]["name"].as_str().unwrap();
        let args = call["function"]["arguments"].as_str().unwrap();
        send_event(
            out,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
            }),
        )
        .await;
        send_event(
            out,
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "input_json_delta", "partial_json": args }
            }),
        )
        .await;
        send_event(
            out,
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": index }),
        )
        .await;
        index += 1;
    }

    let reason = finish_reason.unwrap_or(if calls.is_empty() {
        "end_turn"
    } else {
        "tool_use"
    });
    send_event(
        out,
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": reason },
            "usage": { "output_tokens": completion }
        }),
    )
    .await;
    send_event(out, "message_stop", json!({ "type": "message_stop" })).await;
}

async fn send_event(out: &mut TcpStream, event: &str, data: Value) {
    out.write_all(format!("event: {event}\ndata: {data}\n\n").as_bytes())
        .await
        .unwrap();
    out.flush().await.unwrap();
}

/// `text` in pieces of [`PIECE`] characters.
fn pieces(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(PIECE).map(|c| c.iter().collect()).collect()
}

/// A streamed chunk changing the reply by `delta`.
fn delta(delta: Value, finish_reason: Option<&str>) -> String {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
    })
    .to_string()
}

async fn send(out: &mut TcpStream, data: String) {
    out.write_all(format!("data: {data}\n\n").as_bytes())
        .await
        .unwrap();
    out.flush().await.unwrap();
}
