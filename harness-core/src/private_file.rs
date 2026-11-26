// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The user's files handled with care: the ones that hold secrets or
//! state, such as API keys, OAuth tokens or sessions, and the ones read
//! up to a size.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind, Read};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;

use crate::Error;

/// Create the folder `path` and the missing ones above it, for the user
/// only (`0700`).
pub fn create_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// `path` with `suffix` appended to its file name.
pub(crate) fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    name.into()
}

/// Parse `text`, a TOML file holding secrets. An error tells where, but
/// not the line itself, nor the strings it quotes from it, as the line
/// may hold a key or a token.
pub(crate) fn parse_toml<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    toml::from_str(text).map_err(|e: toml::de::Error| {
        let mut message = String::new();
        for (i, part) in e.message().split('"').enumerate() {
            if i > 0 {
                message.push('"');
            }
            message.push_str(if i % 2 == 1 { "..." } else { part });
        }
        let Some(span) = e.span() else {
            return message;
        };
        let before = text.get(..span.start).unwrap_or_default();
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!("line {line}, column {column}: {message}")
    })
}

/// Replace the file at `path` with what `write` writes, for the user only
/// (`0600`). It is written next to `path`, under a name of this process,
/// and takes its place only once complete and on disk, so a failure
/// leaves `path` as it was, and so does another process replacing it.
pub(crate) fn replace(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let tmp = sibling(path, &format!(".{}.tmp", std::process::id()));
    // One left by a failure of this process.
    let _ = fs::remove_file(&tmp);
    let res = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| {
            write(&mut file)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, path));
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
        return res;
    }
    // The new name is on disk once its folder is. The file is in place
    // either way.
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        let _ = File::open(dir).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

/// Read the private file at `path`, at most `max_bytes` long.
///
/// Returns `None` when the file does not exist; rejects non-regular files,
/// oversized files, and permissions accessible by group or others
/// (`mode & 0o077 != 0`).
pub(crate) fn read(path: &Path, max_bytes: usize) -> Result<Option<String>, Error> {
    let error = |e: &dyn fmt::Display| format!("{}: {e}", path.display());
    let too_big = || error(&format!("file exceeds {} KiB limit", max_bytes >> 10));
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(error(&e).into()),
    };
    let meta = file.metadata().map_err(|e| error(&e))?;
    if !meta.is_file() {
        return Err(error(&"not a regular file").into());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(error(&format!(
            "permissions {mode:04o} are too open; run `chmod 600 {}`",
            path.display()
        ))
        .into());
    }
    if meta.len() > max_bytes as u64 {
        return Err(too_big().into());
    }

    let mut raw = String::new();
    file.take(max_bytes as u64 + 1)
        .read_to_string(&mut raw)
        .map_err(|e| error(&e))?;
    if raw.len() > max_bytes {
        return Err(too_big().into());
    }
    Ok(Some(raw))
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
