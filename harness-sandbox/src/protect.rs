// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Protecting the harness from the commands it runs.

use std::io;

/// Keep other processes of the same user, such as the commands the harness
/// runs, from reading the memory and environment of the calling process,
/// where API keys live, or from tracing it.
///
/// Children inherit this until `exec`.
pub fn protect_self() -> io::Result<()> {
    set_dumpable(false)
}

/// Set whether processes of the same user may inspect the calling process.
pub(crate) fn set_dumpable(dumpable: bool) -> io::Result<()> {
    nix::sys::prctl::set_dumpable(dumpable).map_err(io::Error::from)
}
