// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Protecting the harness from the commands it runs.

use std::io;

use nix::unistd::Uid;

/// Refuse to run as root, or with sudo.
///
/// The container maps the user to itself, so root keeps every capability
/// in it: commands could unmount the masks over denied paths, or read and
/// write any file of root that a shown folder holds.
pub fn refuse_root() -> io::Result<()> {
    if Uid::current().is_root() || Uid::effective().is_root() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to run as root or with sudo: run kl as your own user",
        ));
    }
    Ok(())
}

/// Keep other processes of the same user, such as the commands the harness
/// runs, from reading the memory and environment of the calling process,
/// where API keys live, or from tracing it.
///
/// Children inherit this until `exec`.
pub fn protect_self() -> io::Result<()> {
    nix::sys::prctl::set_dumpable(false).map_err(io::Error::from)
}

/// Make the calling process and those it starts the first ones the kernel
/// kills when memory runs out, before the user's other programs: the
/// biggest of them goes, usually a runaway command.
///
/// Raising it needs no privilege. Commands may lower it back to the usual
/// value, which still leaves the biggest process to go.
pub fn kill_first_on_oom() -> io::Result<()> {
    std::fs::write("/proc/self/oom_score_adj", "1000")
}
