// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Default system prompt and `AGENTS.md` instruction discovery.

use std::collections::HashSet;
use std::fmt::Write;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;

use crate::skills::Skill;

/// File name searched for project and global agent instructions.
pub const AGENTS_FILE: &str = "AGENTS.md";

/// Maximum bytes read from a single `AGENTS.md` file.
pub const MAX_AGENTS_BYTES: usize = 64 * 1024;

/// Maximum bytes read across all `AGENTS.md` files combined.
const MAX_TOTAL_AGENTS_BYTES: usize = 256 * 1024;

/// Discover `AGENTS.md` files for `cwd`, ordered from broadest to most
/// specific:
///
/// 1. `global`, if provided and pointing to an existing file (e.g.
///    `~/.kluis/AGENTS.md`).
/// 2. Every `AGENTS.md` from the enclosing git repository root down to `cwd`
///    (inclusive), or only `cwd/AGENTS.md` when `cwd` is not inside a git
///    repository.
///
/// Duplicate paths (e.g. when `cwd` is the global folder) are omitted while
/// preserving order.
///
/// A project `AGENTS.md` must be a regular file, not a symlink: the
/// sandboxed model can write there, and could link it to a file the sandbox
/// hides, such as `~/.kluis/auth.toml`, for the host to read. The global
/// one, which only the user writes, may be a symlink.
pub fn find_agents_files(cwd: &Path, global: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(path) = global {
        let path = if path.is_symlink() {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        } else {
            path.to_path_buf()
        };
        push_if_file(path, &mut seen, &mut out);
    }

    for dir in project_dirs(cwd) {
        push_if_file(dir.join(AGENTS_FILE), &mut seen, &mut out);
    }

    out
}

/// The folders from the enclosing git repository root down to `cwd`
/// (inclusive), or only `cwd` when it is not inside a git repository.
fn project_dirs(cwd: &Path) -> Vec<&Path> {
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir);
        if dir.join(".git").exists() {
            dirs.reverse();
            return dirs;
        }
    }
    vec![cwd]
}

/// Build the default system prompt for `cwd`, appending any readable,
/// non-empty `AGENTS.md` files in `agents_files` and the list of `skills`.
pub fn system_prompt(cwd: &Path, agents_files: &[PathBuf], skills: &[Skill]) -> String {
    let mut prompt = format!(
        "You are a coding assistant running in a Linux terminal.\n\
         \n\
         Guidelines:\n\
         - Use `read`, `grep`, `find`, and `ls` to inspect files instead of shell commands when possible.\n\
         - Use `edit` and `write` to modify files, and read a file before editing it.\n\
         - Use `bash` to run builds, tests, git, and other command-line tools.\n\
         - Use `todo` to plan and track multi-step tasks.\n\
         - Keep changes minimal and focused, and verify them when appropriate.\n\
         - Be concise and direct; report errors or blockers clearly.\n\
         \n\
         Environment:\n\
         - Working directory: {}\n\
         - OS: {} ({})",
        cwd.display(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    let mut appended_header = false;
    let mut remaining = MAX_TOTAL_AGENTS_BYTES;
    for path in agents_files {
        if remaining == 0 {
            break;
        }
        let limit = MAX_AGENTS_BYTES.min(remaining);
        let Some((bytes, truncated)) = read_capped(path, limit) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        remaining = remaining.saturating_sub(bytes.len());
        if !appended_header {
            prompt.push_str("\n\n## Instructions");
            appended_header = true;
        }
        let _ = write!(prompt, "\n\n### {}\n\n{trimmed}", path.display());
        if truncated {
            prompt.push_str("\n… (truncated)");
        }
    }

    if !skills.is_empty() {
        prompt.push_str(
            "\n\n## Skills\n\n\
             Skills hold instructions for specific tasks. When a task matches a skill, \
             read its file first and follow it; paths in it are relative to its folder. \
             A message starting with `/<name>` asks to use that skill.\n",
        );
        for skill in skills {
            let _ = write!(
                prompt,
                "\n- {}: {} ({})",
                skill.name,
                skill.description,
                skill.path.display()
            );
        }
    }

    prompt
}

/// Add `path` when it is a regular file, not a symlink to one.
fn push_if_file(path: PathBuf, seen: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    if !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
        return;
    }
    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if seen.insert(key) {
        out.push(path);
    }
}

/// Up to `limit` bytes of the regular file `path`, and whether it holds
/// more.
pub(crate) fn read_capped(path: &Path, limit: usize) -> Option<(Vec<u8>, bool)> {
    // Never through a symlink, even one swapped in after the file was
    // found; non-blocking, so a FIFO cannot hang the open.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let cap = usize::try_from(meta.len()).unwrap_or(limit).min(limit + 1);
    let mut bytes = Vec::with_capacity(cap);
    file.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    let truncated = bytes.len() > limit;
    if truncated {
        bytes.truncate(limit);
        strip_incomplete_utf8_tail(&mut bytes);
    }
    Some((bytes, truncated))
}

fn strip_incomplete_utf8_tail(bytes: &mut Vec<u8>) {
    let start = bytes.len().saturating_sub(3);
    if let Some(pos) = bytes[start..].iter().rposition(|&b| b & 0xC0 != 0x80) {
        let lead = start + pos;
        if let Err(err) = std::str::from_utf8(&bytes[lead..]) {
            if err.error_len().is_none() {
                bytes.truncate(lead);
            }
        }
    }
}
