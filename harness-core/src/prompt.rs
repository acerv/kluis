// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Default system prompt and `AGENTS.md` instruction discovery.

use std::collections::HashSet;
use std::fmt::Write;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;

use crate::private_file::read_capped;
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

/// The `AGENTS.md` of the project `cwd` is in: the one of the enclosing git
/// repository root, or of `cwd` outside a git repository.
pub fn project_agents_file(cwd: &Path) -> PathBuf {
    project_dirs(cwd)[0].join(AGENTS_FILE)
}

/// Add the instruction `text` to the `AGENTS.md` at `path` as a list item
/// on a line of its own, creating the file when missing. Returns the line
/// added, without its newline.
///
/// A project is writable by the model: a symlink or a FIFO it left at
/// `path` is refused rather than followed or waited on. Resolve the global
/// file, which may be a symlink of the user, before passing it.
pub fn remember(path: &Path, text: &str) -> io::Result<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err(io::Error::other("nothing to remember"));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o644)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    // Start on a new line when the file does not end with one.
    let mut last = *b"\n";
    if file.seek(SeekFrom::End(-1)).is_ok() {
        file.read_exact(&mut last)?;
    }
    let line = format!("- {text}");
    let start = if last == *b"\n" { "" } else { "\n" };
    file.write_all(format!("{start}{line}\n").as_bytes())?;
    Ok(line)
}

/// Build the default system prompt for `cwd`, appending any readable,
/// non-empty `AGENTS.md` files in `agents_files` and the list of `skills`.
pub fn system_prompt(cwd: &Path, agents_files: &[PathBuf], skills: &[Skill]) -> String {
    let mut prompt = format!(
        "You are a coding assistant running in a Linux terminal.\n\
         \n\
         Guidelines:\n\
         - Use `read`, `grep`, `find`, and `ls` to inspect files instead of shell commands.\n\
         - Use `edit` and `write` to modify files, and read a file before editing it.\n\
         - Use `bash` to run builds, tests, git, and other command-line tools.\n\
         - Use `todo` to plan multi-step tasks, and update it as you go: mark each item completed once done, before starting the next.\n\
         - Use `task` for a part of a large task that takes many steps, such as a wide search: only the answer comes back.\n\
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
             Skills hold the user's instructions for specific tasks. Before starting a \
             task that matches a skill below, call the `skill` tool with its name and \
             follow the instructions it returns. A message starting with `/<name>` \
             asks for that skill.\n",
        );
        for skill in skills {
            let _ = write!(prompt, "\n- {}: {}", skill.name, skill.description);
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
