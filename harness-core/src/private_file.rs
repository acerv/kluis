// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Reading files that hold secrets, such as API keys or OAuth tokens.

use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::Error;

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
