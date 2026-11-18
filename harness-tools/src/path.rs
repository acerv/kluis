// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::ffi::OsString;
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
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    expand_with(path, std::env::var_os("HOME"))
}

fn expand_with(path: &str, home: Option<OsString>) -> PathBuf {
    let rest = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return PathBuf::from(path),
    };
    match home {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(path: &str, home: Option<&str>) -> PathBuf {
        expand_with(path, home.map(OsString::from))
    }

    #[test]
    fn expands_home() {
        assert_eq!(exp("~", Some("/h")), PathBuf::from("/h"));
        assert_eq!(exp("~/", Some("/h")), PathBuf::from("/h"));
        assert_eq!(exp("~/a/b", Some("/h")), PathBuf::from("/h/a/b"));
    }

    #[test]
    fn leaves_other_paths() {
        assert_eq!(exp("~user/a", Some("/h")), PathBuf::from("~user/a"));
        assert_eq!(exp("a/~/b", Some("/h")), PathBuf::from("a/~/b"));
        assert_eq!(exp("/abs", Some("/h")), PathBuf::from("/abs"));
        assert_eq!(exp("", Some("/h")), PathBuf::from(""));
    }

    #[test]
    fn no_home() {
        assert_eq!(exp("~/a", None), PathBuf::from("~/a"));
        assert_eq!(exp("~/a", Some("")), PathBuf::from("~/a"));
    }
}
