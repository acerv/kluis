// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Bounded NDJSON wire protocol between host and sandboxed tool worker.

use std::io::{self, BufRead, Read};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

use crate::ToolDetails;

/// Maximum size of a single NDJSON protocol frame (8 MiB).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Capacity threshold above which reusable frame buffers shrink back down so a
/// single large payload does not stay pinned in memory.
pub const RETAINED_BUF_CAP: usize = 64 * 1024;

/// Messages sent from the host to the sandboxed tool worker over `stdin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Call {
        id: u64,
        name: String,
        args: Value,
    },
    Cancel {
        id: u64,
    },
    /// Stop what commands left running in the background, but the
    /// commands started in the background with `keep_jobs`. Answered once
    /// they are gone by `{"jobs": true}` when such commands were running.
    StopBackground {
        id: u64,
        #[serde(default)]
        keep_jobs: bool,
    },
}

/// Messages sent from the sandboxed tool worker to the host over `stdout`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    /// Emitted once on worker startup once the container is live.
    Ready,
    Output {
        id: u64,
        chunk: String,
    },
    Result {
        id: u64,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<ToolDetails>,
    },
}

/// Shrink `buf` back to empty if a previous frame grew its capacity past
/// [`RETAINED_BUF_CAP`].
pub fn reset_buf(buf: &mut Vec<u8>) {
    if buf.capacity() > RETAINED_BUF_CAP {
        *buf = Vec::new();
    } else {
        buf.clear();
    }
}

/// `value` as one NDJSON line, failing if the other side would not read
/// it, as it exceeds [`MAX_FRAME_BYTES`].
pub fn frame(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut buf =
        serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    check_frame_len(buf.len())?;
    Ok(buf)
}

/// Read one `\n`-terminated line into `buf`, failing if it exceeds
/// [`MAX_FRAME_BYTES`]. Returns `0` on EOF.
pub fn read_line_bounded(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<usize> {
    reset_buf(buf);
    let n = reader
        .by_ref()
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', buf)?;
    check_frame_len(n)
}

/// Async version of [`read_line_bounded`].
pub async fn read_line_bounded_async<R>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    reset_buf(buf);
    let n = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', buf)
        .await?;
    check_frame_len(n)
}

fn check_frame_len(n: usize) -> io::Result<usize> {
    if n > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("protocol frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    Ok(n)
}
