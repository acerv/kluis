// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A fake OpenAI-compatible model server for agent tests.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// What the server answers to a request.
pub enum Reply {
    /// A chat completion body.
    Json(Value),
    /// No answer at all, as a model that takes forever.
    Hang,
}

/// A reply with only text.
pub fn text(text: &str) -> Reply {
    completion(json!({ "role": "assistant", "content": text }), "stop")
}

/// A reply calling tools, given as `(id, name, arguments)`.
pub fn tool_calls(calls: &[(&str, &str, Value)]) -> Reply {
    let calls: Vec<Value> = calls
        .iter()
        .map(|(id, name, args)| {
            json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args.to_string() }
            })
        })
        .collect();
    completion(
        json!({ "role": "assistant", "content": null, "tool_calls": calls }),
        "tool_calls",
    )
}

fn completion(message: Value, finish_reason: &str) -> Reply {
    Reply::Json(json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test",
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    }))
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
        let url = self.url.clone();
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(url.clone()),
                    auth: AuthData::from_single("test"),
                    model: ModelIden::new(AdapterKind::OpenAI, target.model.model_name),
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

/// Answer the requests of one connection, which the client may keep open.
async fn serve(
    stream: TcpStream,
    seen: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
) {
    let mut stream = BufReader::new(stream);
    loop {
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
        let body = match reply {
            Some(Reply::Json(body)) => body.to_string(),
            Some(Reply::Hang) => std::future::pending().await,
            None => panic!("no reply left for this request"),
        };
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        let out = stream.get_mut();
        out.write_all(head.as_bytes()).await.unwrap();
        out.write_all(body.as_bytes()).await.unwrap();
    }
}
