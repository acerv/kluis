// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Where the web tools may connect.
//!
//! The web tools run on the host, so a URL from the model could reach
//! services the sandbox hides: localhost, the LAN or a cloud metadata
//! endpoint. Their client connects only to public addresses, checked on the
//! addresses a name resolves to, so a name cannot be pointed at a private
//! address after the check, and on every redirect. A host list, when given,
//! limits the client to those hosts, which may then be private.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{redirect, Client, Response, Url};

use crate::error::chain;
use crate::fetch::{is_web, FETCH_TIMEOUT};
use crate::tools::{Hosts, ToolError};

/// Redirects followed by one request.
const MAX_REDIRECTS: usize = 10;

/// An HTTP client for the web tools, refusing private addresses and hosts
/// left out of its host list.
#[derive(Debug, Clone)]
pub struct WebClient {
    client: Client,
    hosts: Arc<Hosts>,
}

impl WebClient {
    /// A client for `hosts` and their subdomains, at any address, or for
    /// any host at a public address when `hosts` is empty.
    pub fn new<S: AsRef<str>>(hosts: &[S]) -> Result<Self, ToolError> {
        let hosts = Arc::new(Hosts::new(hosts)?);
        let redirects = hosts.clone();
        let client = Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(concat!("kluis/", env!("CARGO_PKG_VERSION")))
            // A proxy would resolve names itself, out of reach of the
            // checks.
            .no_proxy()
            .dns_resolver(Resolver(hosts.clone()))
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.error("too many redirects")
                } else if let Err(e) = check(&redirects, attempt.url()) {
                    attempt.error(e)
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| ToolError::new(chain(&e)))?;
        Ok(Self { client, hosts })
    }

    /// The response to a GET of `url`.
    pub(crate) async fn get(&self, url: Url) -> Result<Response, String> {
        check(&self.hosts, &url)?;
        self.client.get(url).send().await.map_err(|e| chain(&e))
    }
}

/// Why `url` may not be asked of a client limited to `hosts`, any
/// public one when empty, if it may not.
fn check(hosts: &Hosts, url: &Url) -> Result<(), String> {
    if !is_web(url) {
        return Err("only http and https URLs are allowed".into());
    }
    let host = url.host_str().unwrap_or_default();
    if hosts.lists(host) {
        return Ok(());
    }
    if !hosts.is_empty() {
        return Err(format!("{host} is not an allowed host"));
    }
    match host.trim_matches(['[', ']']).parse() {
        Ok(ip) if !is_public(ip) => Err(format!("{host} is not a public address")),
        _ => Ok(()),
    }
}

/// Resolves names to their public addresses only, unless they are listed.
struct Resolver(Arc<Hosts>);

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let hosts = self.0.clone();
        Box::pin(async move {
            let host = name.as_str();
            let listed = hosts.lists(host);
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, 0))
                .await?
                .filter(|addr| listed || is_public(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Whether `ip` is a public unicast address: not loopback, private,
/// link-local, shared, multicast, reserved or for documentation. An IPv4
/// address inside an IPv6 one is judged as IPv4.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match embedded_v4(ip) {
            Some(ip) => is_public_v4(ip),
            None => {
                let [a, b, ..] = ip.segments();
                // Global unicast, less Teredo and documentation.
                a & 0xe000 == 0x2000 && !(a == 0x2001 && (b == 0 || b == 0xdb8))
            }
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_documentation()
        // Shared address space, used by carrier-grade NAT.
        || (a == 100 && b & 0xc0 == 64)
        // IETF protocol assignments.
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking.
        || (a == 198 && b & 0xfe == 18)
        // Multicast, reserved and broadcast.
        || a >= 224)
}

/// The IPv4 address carried by `ip`: IPv4-mapped, IPv4-compatible (`::`
/// and `::1` included), NAT64 or 6to4.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let v4 = |hi: u16, lo: u16| Some(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo)));
    match s {
        [0, 0, 0, 0, 0, 0 | 0xffff, hi, lo] | [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => v4(hi, lo),
        [0x2002, hi, lo, ..] => v4(hi, lo),
        _ => None,
    }
}
