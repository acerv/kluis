// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A sandbox told in words, for the user to check before trusting it.

use std::fmt::Write;

use crate::mounts::MountKind;
use crate::{Network, Sandbox, SandboxError};

impl Sandbox {
    /// What [`Sandbox::container`] sets up, as text: the paths commands can
    /// change, only read, and the ones hidden from them, in the order they
    /// are mounted, then the empty folders, the network and the limits.
    ///
    /// It is made from the mounts the container gets, and fails as
    /// building the container does.
    pub fn describe(&self) -> Result<String, SandboxError> {
        let mounts = self.mounts()?;
        let paths = |wanted: fn(MountKind) -> bool| {
            mounts
                .iter()
                .filter(move |(_, kind)| wanted(*kind))
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>()
        };
        let size = self.max_tmp_size.map_or(String::new(), |bytes| {
            format!(" (up to {})", format_size(bytes))
        });
        let private = std::iter::once("/tmp".to_owned())
            .chain(self.private.iter().map(|p| p.display().to_string()))
            .map(|path| format!("{path}{size}"))
            .collect();

        let mut out = String::new();
        let sections = [
            (
                "Commands can change",
                paths(|kind| kind == MountKind::Bind { writable: true }),
            ),
            (
                "Commands can only read",
                paths(|kind| kind == MountKind::Bind { writable: false }),
            ),
            (
                "Hidden from commands",
                paths(|kind| matches!(kind, MountKind::MaskDir | MountKind::MaskFile)),
            ),
            ("Empty, kept in memory", private),
        ];
        for (title, paths) in sections {
            if paths.is_empty() {
                continue;
            }
            let _ = writeln!(out, "{title}:");
            for path in paths {
                let _ = writeln!(out, "  {path}");
            }
            out.push('\n');
        }
        let network = match &self.network {
            Network::None => "none".to_owned(),
            Network::Ports(ports) if ports.is_empty() => "none (no TCP connection)".to_owned(),
            Network::Ports(ports) => {
                let ports: Vec<_> = ports.iter().map(u16::to_string).collect();
                format!("TCP ports {} on any host", ports.join(", "))
            }
            Network::Any => "any".to_owned(),
        };
        let _ = writeln!(out, "Network: {network}");
        if let Some(max) = self.max_processes {
            let _ = writeln!(out, "Processes and threads: at most {max}");
        }
        Ok(out)
    }
}

/// `bytes` in the largest binary unit that keeps it above one.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} bytes"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}
