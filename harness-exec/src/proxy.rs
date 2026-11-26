// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! An HTTP proxy on the host letting commands reach only some hosts.
//!
//! It tunnels `CONNECT` requests, as for HTTPS, and forwards plain HTTP
//! ones, to the listed hosts and their subdomains only. It reads no
//! TLS: it limits where commands connect, not what they send there.

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Semaphore};
use tokio::time::{sleep, sleep_until, timeout, Instant};
use url::Url;

use crate::{lock, Hosts};

/// Connections served at once; others wait.
const MAX_CONNECTIONS: usize = 64;

/// Bytes of a request head, the request line and headers.
const MAX_HEAD: usize = 16 * 1024;

/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

/// How long connecting to a host may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a connection may stay quiet both ways.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Refused hosts kept until [`Proxy::take_refused`].
const MAX_REFUSED: usize = 16;

/// A proxy serving one listener on a thread of its own, until dropped.
#[derive(Debug)]
pub struct Proxy {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    refused: Arc<Mutex<Vec<String>>>,
}

impl Proxy {
    /// Serve `listener`, letting clients reach only `hosts`.
    pub fn start(listener: std::net::TcpListener, hosts: Hosts) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop, stopped) = oneshot::channel();
        let refused = Arc::<Mutex<Vec<String>>>::default();
        let refused_bg = refused.clone();
        let thread = thread::Builder::new().name("proxy".into()).spawn(move || {
            runtime.block_on(async move {
                let Ok(listener) = TcpListener::from_std(listener) else {
                    return;
                };
                tokio::select! {
                    _ = stopped => {}
                    () = serve(listener, Arc::new(hosts), refused_bg) => {}
                }
            });
            // Closes every connection left, without waiting for a name
            // still being resolved.
            runtime.shutdown_background();
        })?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
            refused,
        })
    }

    /// The hosts refused since the last call, each once.
    pub fn take_refused(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.refused))
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Accept connections and serve each, at most [`MAX_CONNECTIONS`] at once.
async fn serve(listener: TcpListener, hosts: Arc<Hosts>, refused: Arc<Mutex<Vec<String>>>) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(permit) = permits.clone().acquire_owned().await else {
            return;
        };
        let client = match listener.accept().await {
            Ok((client, _)) => client,
            // Such as too many open files: wait for some to close.
            Err(_) => {
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (hosts, refused) = (hosts.clone(), refused.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let _ = handle(client, &hosts, &refused).await;
        });
    }
}

/// Why a request is not forwarded.
enum Refusal {
    /// The host it names is not listed.
    Host(String),
    /// Something else failed: the status sent back, and the reason.
    Failed(&'static str, String),
}

/// Where a request goes, and what to send there first.
struct Target {
    host: String,
    port: u16,
    /// Bytes for the host before relaying: the head rewritten for it, if
    /// any, and what the client sent after its head.
    first: Vec<u8>,
    /// Whether the client asked for a tunnel, which it awaits a reply to.
    tunnel: bool,
}

/// Serve one client: read its request, then relay it to the host it names,
/// or refuse it.
async fn handle(
    mut client: TcpStream,
    hosts: &Hosts,
    refused: &Mutex<Vec<String>>,
) -> io::Result<()> {
    let mut buf = Vec::new();
    let head = timeout(HEAD_TIMEOUT, read_head(&mut client, &mut buf)).await??;
    let target = match target(&buf, head, hosts) {
        Ok(target) => target,
        Err(refusal) => {
            if let Refusal::Host(host) = &refusal {
                note_refused(refused, host);
            }
            return refuse(&mut client, &refusal).await;
        }
    };
    let connect = TcpStream::connect((target.host.trim_matches(['[', ']']), target.port));
    let mut server = match timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(server)) => server,
        Ok(Err(err)) => {
            let reason = format!("{}: {err}", target.host);
            return refuse(&mut client, &Refusal::Failed("502 Bad Gateway", reason)).await;
        }
        Err(_) => {
            let reason = format!("{}: timed out", target.host);
            return refuse(&mut client, &Refusal::Failed("504 Gateway Timeout", reason)).await;
        }
    };
    if target.tunnel {
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await?;
    }
    server.write_all(&target.first).await?;
    relay(&mut client, &mut server).await
}

/// Read until the end of the request head into `buf`, and return its
/// length; `buf` may hold more.
async fn read_head(client: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<usize> {
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        // The end may straddle two reads.
        let from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = memchr::memmem::find(&buf[from..], b"\r\n\r\n") {
            return Ok(from + end + 4);
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too long",
            ));
        }
    }
}

/// Where the request whose head is `buf[..head]` goes, if `hosts` let it.
fn target(buf: &[u8], head: usize, hosts: &Hosts) -> Result<Target, Refusal> {
    let bad = |reason: &str| Refusal::Failed("400 Bad Request", reason.to_owned());
    let text = std::str::from_utf8(&buf[..head]).map_err(|_| bad("request head is not UTF-8"))?;
    let (line, headers) = text.split_once("\r\n").unwrap_or((text, ""));
    let mut parts = line.split(' ');
    let (Some(method), Some(uri), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("malformed request line"));
    };
    let tunnel = method.eq_ignore_ascii_case("CONNECT");
    // `CONNECT host:port`, or a plain HTTP request naming its URL.
    let url = if tunnel {
        (!uri.contains(['/', '@']))
            .then(|| Url::parse(&format!("http://{uri}/")).ok())
            .flatten()
    } else {
        Url::parse(uri).ok().filter(|url| url.scheme() == "http")
    };
    let url = url.ok_or_else(|| bad("expected CONNECT host:port or an http:// URL"))?;
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err(bad("no host"));
    };
    if !hosts.lists(host) {
        return Err(Refusal::Host(host.to_owned()));
    }
    let mut first = Vec::new();
    if !tunnel {
        let path = &url[url::Position::BeforePath..url::Position::AfterQuery];
        // The host checked, never another one the client names, which a
        // server for many sites would serve instead.
        let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
        first.extend_from_slice(
            format!("{method} {path} {version}\r\nHost: {authority}\r\n").as_bytes(),
        );
        // One request per connection: the next may be for another host.
        for header in headers.split("\r\n").filter(|h| !h.is_empty()) {
            let name = header.split(':').next().unwrap_or_default().trim();
            let dropped = [
                "host",
                "connection",
                "keep-alive",
                "proxy-connection",
                "proxy-authorization",
            ];
            if !dropped.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                first.extend_from_slice(header.as_bytes());
                first.extend_from_slice(b"\r\n");
            }
        }
        first.extend_from_slice(b"Connection: close\r\n\r\n");
    }
    first.extend_from_slice(&buf[head..]);
    Ok(Target {
        host: host.to_owned(),
        port,
        first,
        tunnel,
    })
}

/// Keep `host` among the refused ones, once.
fn note_refused(refused: &Mutex<Vec<String>>, host: &str) {
    let mut refused = lock(refused);
    if refused.len() < MAX_REFUSED && !refused.iter().any(|h| h == host) {
        refused.push(host.to_owned());
    }
}

/// Answer the client with `refusal`.
async fn refuse(client: &mut TcpStream, refusal: &Refusal) -> io::Result<()> {
    let (status, body) = match refusal {
        Refusal::Host(host) => (
            "403 Forbidden",
            format!("kl: {host} is not an allowed host; allow it with --allow-host\n"),
        ),
        Refusal::Failed(status, reason) => (*status, format!("kl: {reason}\n")),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(response.as_bytes()).await
}

/// Relay bytes both ways until both sides are done, or nothing moves
/// either way for [`IDLE_TIMEOUT`]: no byte read, and none written to a
/// side that stopped reading.
async fn relay(client: &mut TcpStream, server: &mut TcpStream) -> io::Result<()> {
    let (client_read, client_write) = client.split();
    let (server_read, server_write) = server.split();
    // When a byte last moved. Each way runs on its own: one waiting for
    // its reader does not keep the other from going on.
    let moved = Mutex::new(Instant::now());
    let idle = async {
        loop {
            let last = *lock(&moved);
            if last.elapsed() >= IDLE_TIMEOUT {
                break;
            }
            sleep_until(last + IDLE_TIMEOUT).await;
        }
    };
    tokio::select! {
        res = async {
            tokio::try_join!(
                forward(client_read, server_write, &moved),
                forward(server_read, client_write, &moved),
            )
        } => res.map(|_| ()),
        () = idle => Err(io::ErrorKind::TimedOut.into()),
    }
}

/// Write what `from` sends to `to` until it is done sending, which `to`
/// is told, noting in `moved` when bytes went through.
async fn forward(
    mut from: ReadHalf<'_>,
    mut to: WriteHalf<'_>,
    moved: &Mutex<Instant>,
) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = from.read(&mut buf).await?;
        *lock(moved) = Instant::now();
        if n == 0 {
            return to.shutdown().await;
        }
        to.write_all(&buf[..n]).await?;
        *lock(moved) = Instant::now();
    }
}
