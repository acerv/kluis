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
    },
    /// The first pieces of some text, then nothing more.
    Partial(String),
    /// The first pieces of some text, then the connection closes.
    Broken(String),
    /// No answer at all, as a model that takes forever.
    Hang,
}

/// A reply with only text.
pub fn text(text: &str) -> Reply {
    Reply::Turn {
        reasoning: String::new(),
        text: text.into(),
        calls: Vec::new(),
        finish_reason: None,
    }
}

/// `reply`, with the model reasoning first.
pub fn thinking(reasoning: &str, reply: Reply) -> Reply {
    match reply {
        Reply::Turn {
            text,
            calls,
            finish_reason,
            ..
        } => Reply::Turn {
            reasoning: reasoning.into(),
            text,
            calls,
            finish_reason,
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
    let raw: Vec<_> = calls
        .iter()
        .map(|(id, name, args)| (*id, *name, (*args).to_owned()))
        .collect();
    tool_calls_turn(&raw, Some("length".into()))
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
    }
}

/// A server answering chat requests with scripted replies, in order.
pub struct MockModel {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockModel {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));

        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(stream, seen.clone(), replies.clone()));
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
        let url = self.url.clone();
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

    /// Bodies of the requests received so far.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// Answer one request, then close the connection.
async fn serve(
    stream: TcpStream,
    seen: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
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
    let reply = reply.expect("no reply left for this request");
    if let Reply::Hang = reply {
        return std::future::pending().await;
    }

    let out = stream.get_mut();
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    out.write_all(head.as_bytes()).await.unwrap();
    match reply {
        Reply::Turn {
            reasoning,
            text,
            calls,
            finish_reason,
        } => {
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
            let (prompt, completion) = USAGE;
            let usage = json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "total_tokens": prompt + completion
            });
            send(out, json!({ "choices": [], "usage": usage }).to_string()).await;
            send(out, "[DONE]".into()).await;
        }
        Reply::Partial(text) => {
            for piece in pieces(&text) {
                send(out, delta(json!({ "content": piece }), None)).await;
            }
            std::future::pending::<()>().await;
        }
        // Returning drops the connection in the middle of the reply.
        Reply::Broken(text) => {
            for piece in pieces(&text) {
                send(out, delta(json!({ "content": piece }), None)).await;
            }
        }
        Reply::Hang => unreachable!(),
    }
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
