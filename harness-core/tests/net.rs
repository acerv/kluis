// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::net::IpAddr;

use common::web::{Reply, WebServer};
use harness_core::tools::{is_public, FetchTool, WebClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Fetch `url` with a client for `hosts`.
async fn fetch(hosts: &[&str], url: &str) -> Result<String, String> {
    let client = WebClient::new(hosts).unwrap();
    FetchTool {
        url: url.to_owned(),
        raw: true,
        offset: None,
    }
    .run(Some(&client))
    .await
    .map(|out| out.content)
    .map_err(|e| e.message)
}

fn page() -> Reply {
    Reply {
        status: 200,
        content_type: "text/plain",
        body: b"hello".to_vec(),
    }
}

#[test]
fn public_addresses() {
    let public = [
        "1.1.1.1",
        "8.8.8.8",
        "100.128.0.1",
        "172.32.0.1",
        "2606:4700::1111",
        "::ffff:1.1.1.1",
        "64:ff9b::808:808",
        "2002:101:101::1",
    ];
    let private = [
        "0.0.0.0",
        "0.1.2.3",
        "127.0.0.1",
        "127.1.2.3",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "192.0.0.1",
        "192.0.2.1",
        "198.18.0.1",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::127.0.0.1",
        "::ffff:127.0.0.1",
        "::ffff:169.254.169.254",
        "64:ff9b::a00:1",
        "2002:a00:1::1",
        "fe80::1",
        "fc00::1",
        "fd00:ec2::254",
        "fec0::1",
        "ff02::1",
        "2001:db8::1",
        "2001::1",
        "100::1",
    ];
    for ip in public {
        assert!(is_public(ip.parse::<IpAddr>().unwrap()), "{ip}");
    }
    for ip in private {
        assert!(!is_public(ip.parse::<IpAddr>().unwrap()), "{ip}");
    }
}

#[test]
fn hosts_must_be_host_names() {
    for host in [
        "",
        "https://example.com",
        "example.com:80",
        "example.com/a",
        "a b",
    ] {
        let err = WebClient::new(&[host]).unwrap_err();
        assert!(err.message.contains("not a host name"), "{host}: {err}");
    }
    WebClient::new(&["example.com", "EXAMPLE.org", "10.0.0.1", "[::1]"]).unwrap();
}

#[tokio::test]
async fn private_addresses_are_refused() {
    let server = WebServer::serve(vec![page()]).await;
    let port = server.url("").rsplit(':').next().unwrap().to_owned();
    for host in [
        "127.0.0.1",
        "[::1]",
        "[::ffff:127.0.0.1]",
        "0.0.0.0",
        "169.254.169.254",
        "2130706433",
    ] {
        let err = fetch(&[], &format!("http://{host}:{port}/"))
            .await
            .unwrap_err();
        assert!(err.contains("is not a public address"), "{host}: {err}");
    }
    // A name is judged by the addresses it resolves to.
    let err = fetch(&[], &format!("http://localhost:{port}/"))
        .await
        .unwrap_err();
    assert!(err.contains("localhost has no public address"), "{err}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn listed_hosts_may_be_private() {
    let server = WebServer::serve(vec![page(), page()]).await;
    let url = server.url("/");
    assert_eq!(fetch(&["127.0.0.1"], &url).await.unwrap(), "hello");
    let by_name = url.replace("127.0.0.1", "localhost");
    assert_eq!(fetch(&["localhost"], &by_name).await.unwrap(), "hello");
}

#[tokio::test]
async fn only_listed_hosts() {
    for url in [
        "http://example.org/",
        "http://notexample.com/",
        "http://example.com.example.org/",
        "http://1.1.1.1/",
    ] {
        let err = fetch(&["example.com"], url).await.unwrap_err();
        assert!(err.contains("is not an allowed host"), "{url}: {err}");
    }
}

#[tokio::test]
async fn redirects_are_checked() {
    // A listed host sending the client to one that is not.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = stream.read(&mut [0; 4096]).await;
        let head = format!(
            "HTTP/1.1 302 X\r\nlocation: http://127.0.0.1:{port}/\r\n\
             content-length: 0\r\nconnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await.unwrap();
    });
    let err = fetch(&["localhost"], &format!("http://localhost:{port}/"))
        .await
        .unwrap_err();
    assert!(err.contains("127.0.0.1 is not an allowed host"), "{err}");
}
