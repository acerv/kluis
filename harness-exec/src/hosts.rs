// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Lists of hosts the user lets something reach.

use url::Url;

use crate::ToolError;

/// Host names, each allowing itself and its subdomains.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hosts(Vec<String>);

impl Hosts {
    /// The list of `hosts`, each a host alone as written in a URL: no
    /// scheme, port or path.
    pub fn new<S: AsRef<str>>(hosts: &[S]) -> Result<Self, ToolError> {
        hosts
            .iter()
            .map(|host| {
                let host = host.as_ref();
                Url::parse(&format!("http://{host}/"))
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .filter(|name| name.eq_ignore_ascii_case(host))
                    .ok_or_else(|| ToolError::new(format!("{host} is not a host name")))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `host`, as in a URL, is listed or a subdomain of a listed
    /// host.
    pub fn lists(&self, host: &str) -> bool {
        self.0.iter().any(|allowed| {
            host.strip_suffix(allowed.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'))
        })
    }
}
