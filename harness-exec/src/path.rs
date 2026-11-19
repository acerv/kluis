// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::path::{Path, PathBuf};

/// Where `path` points when seen from `cwd`: a leading `~` expands to
/// `$HOME`, and relative paths start at `cwd`.
pub(crate) fn resolve(cwd: &Path, path: &str) -> PathBuf {
    // Joining an absolute path replaces `cwd`.
    cwd.join(expand_tilde(path))
}

/// Expand a leading `~` or `~/` to `$HOME`.
///
/// `~user` forms are left untouched, as is everything when `$HOME` is
/// not set.
fn expand_tilde(path: &str) -> PathBuf {
    let rest = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return PathBuf::from(path),
    };
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let mut out = PathBuf::from(home);
            let rest = rest.trim_start_matches('/');
            if !rest.is_empty() {
                out.push(rest);
            }
            out
        }
        _ => PathBuf::from(path),
    }
}
