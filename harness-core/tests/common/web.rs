// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A fake HTTP server answering queued replies, one per connection,
//! and recording every request it receives.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// One request received by [`WebServer`].
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path with its query string.
    pub path: String,
    /// Headers with lowercase names.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// A reply of [`WebServer`].
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

pub struct WebServer {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl WebServer {
    /// Serve `replies` (status, JSON body) in order; then answer 500.
    pub async fn start(replies: Vec<(u16, Value)>) -> Self {
        let replies = replies.into_iter().map(|(status, body)| Reply {
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
        });
        Self::serve(replies.collect()).await
    }

    /// Serve `replies` in order; then answer 500.
    pub async fn serve(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let mut replies = VecDeque::from(replies);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let reply = replies.pop_front().unwrap_or(Reply {
                    status: 500,
                    content_type: "text/plain",
                    body: Vec::new(),
                });
                serve(stream, reply, &log).await;
            }
        });
        Self { addr, requests }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve(stream: TcpStream, reply: Reply, log: &Mutex<Vec<Request>>) -> Option<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();

    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.ok()?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut raw = vec![0; len];
    reader.read_exact(&mut raw).await.ok()?;

    log.lock().unwrap().push(Request {
        method,
        path,
        headers,
        body: String::from_utf8(raw).ok()?,
    });

    let head = format!(
        "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    let stream = reader.get_mut();
    stream.write_all(head.as_bytes()).await.ok()?;
    stream.write_all(&reply.body).await.ok()
}
