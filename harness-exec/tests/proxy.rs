// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::thread;

use harness_exec::{Hosts, Proxy};

/// A proxy for `hosts`, and the port it listens on.
fn proxy(hosts: &[&str]) -> (Proxy, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (
        Proxy::start(listener, Hosts::new(hosts).unwrap()).unwrap(),
        port,
    )
}

/// A server answering one connection: it reads until the client is done
/// sending, then answers "pong". Returns its port and what it read.
fn server() -> (u16, thread::JoinHandle<String>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        stream.write_all(b"pong").unwrap();
        text
    });
    (port, handle)
}

/// Send `request` to the proxy at `port`, stop sending, and read the
/// answer.
fn ask(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    answer
}

#[test]
fn tunnels_to_listed_hosts() {
    let (_proxy, port) = proxy(&["localhost"]);
    let (target, server) = server();
    let answer = ask(
        port,
        &format!("CONNECT localhost:{target} HTTP/1.1\r\nHost: x\r\n\r\nping"),
    );
    assert_eq!(answer, "HTTP/1.1 200 Connection established\r\n\r\npong");
    assert_eq!(server.join().unwrap(), "ping");
}

#[test]
fn forwards_plain_http_requests_one_per_connection() {
    let (_proxy, port) = proxy(&["127.0.0.1"]);
    let (target, server) = server();
    let answer = ask(
        port,
        &format!(
            "GET http://127.0.0.1:{target}/crates?q=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Proxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n"
        ),
    );
    assert_eq!(answer, "pong");
    assert_eq!(
        server.join().unwrap(),
        "GET /crates?q=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
}

#[test]
fn refuses_hosts_not_listed() {
    let (proxy, port) = proxy(&["localhost"]);
    for request in [
        "CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n",
        "CONNECT notlocalhost:443 HTTP/1.1\r\n\r\n",
        "GET http://example.com/ HTTP/1.1\r\n\r\n",
    ] {
        let answer = ask(port, request);
        assert!(answer.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{answer}");
        assert!(answer.contains("is not an allowed host"), "{answer}");
    }
    assert_eq!(
        proxy.take_refused(),
        ["127.0.0.1", "notlocalhost", "example.com"]
    );
    assert!(proxy.take_refused().is_empty());
}

#[test]
fn refuses_requests_it_cannot_route() {
    let (proxy, port) = proxy(&["localhost"]);
    for request in [
        "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET https://localhost/ HTTP/1.1\r\n\r\n",
        "CONNECT user@localhost:443 HTTP/1.1\r\n\r\n",
        "nonsense\r\n\r\n",
    ] {
        let answer = ask(port, request);
        assert!(
            answer.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{answer}"
        );
    }
    assert!(proxy.take_refused().is_empty());
}

#[test]
fn reports_hosts_it_cannot_reach() {
    let (_proxy, port) = proxy(&["localhost"]);
    // A port nothing listens on.
    let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let answer = ask(
        port,
        &format!("CONNECT localhost:{closed} HTTP/1.1\r\n\r\n"),
    );
    assert!(
        answer.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
        "{answer}"
    );
}

#[test]
fn stops_when_dropped() {
    let (proxy, port) = proxy(&["localhost"]);
    drop(proxy);
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
}

#[test]
fn a_side_that_stops_reading_does_not_hold_the_other_way() {
    let (_proxy, port) = proxy(&["127.0.0.1"]);
    // A server that never reads, and answers after a while.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = listener.local_addr().unwrap().port();
    let (sent, wait) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        wait.recv().unwrap();
        stream.write_all(b"pong").unwrap();
        // Kept open until the client got the answer.
        let _ = wait.recv();
    });

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    write!(client, "CONNECT 127.0.0.1:{target} HTTP/1.1\r\n\r\n").unwrap();
    let mut established = [0u8; 39];
    client.read_exact(&mut established).unwrap();

    // Send until the proxy takes no more, waiting for the server to read.
    let timeout = Some(std::time::Duration::from_millis(500));
    client.set_write_timeout(timeout).unwrap();
    while client.write(&[0u8; 65536]).is_ok() {}
    sent.send(()).unwrap();

    client
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut answer = [0u8; 4];
    client.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"pong");
    drop(sent);
    server.join().unwrap();
}
