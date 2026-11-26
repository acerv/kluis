// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The commands running in the background.

use std::fs::File;
use std::io::{self, Seek, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;
use tokio::process::Child;

use super::output::{Filter, Tail};
use super::MAX_BASH_LOG_BYTES;
use crate::lock;

/// Commands started in the background, by the pid of their process
/// group; the groups still running are killed once the last clone is
/// dropped.
#[derive(Clone, Debug, Default)]
pub struct Jobs(Arc<JobList>);

#[derive(Debug, Default)]
struct JobList {
    pids: Mutex<Vec<u32>>,
    /// Whether a command ended by itself since [`Jobs::stop_unless`].
    ended: AtomicBool,
}

impl Drop for JobList {
    fn drop(&mut self) {
        if let Ok(pids) = self.pids.get_mut() {
            pids.drain(..).for_each(|pid| kill_group(Some(pid)));
        }
    }
}

impl Jobs {
    /// The process groups of the commands still running, which are also
    /// their sessions.
    pub fn pids(&self) -> Vec<u32> {
        lock(&self.0.pids).clone()
    }

    /// Kill every command still running.
    pub fn stop(&self) {
        lock(&self.0.pids)
            .drain(..)
            .for_each(|pid| kill_group(Some(pid)));
    }

    /// Follow a command just started: its cleaned output goes to `file`
    /// until it ends.
    pub(super) fn watch(&self, pid: u32, child: Child, pipe: Receiver, file: File) {
        lock(&self.0.pids).push(pid);
        tokio::spawn(log_job(child, pipe, file, Arc::downgrade(&self.0)));
    }

    /// Kill every command still running unless `keep`, and return whether
    /// any was, or ended by itself since the last call.
    pub fn stop_unless(&self, keep: bool) -> bool {
        // A command gone from the list has already marked it ended.
        let running = !self.pids().is_empty();
        let running = self.0.ended.swap(false, Ordering::AcqRel) || running;
        if !keep {
            self.stop();
        }
        running
    }
}

/// Write the cleaned output of a background command to `file` until it
/// ends, then take it off `jobs`.
async fn log_job(mut child: Child, mut pipe: Receiver, mut file: File, jobs: Weak<JobList>) {
    let pid = child.id();
    let log = async {
        let mut filter = Filter::default();
        let mut text = Tail::default();
        let mut chunk = [0u8; 8192];
        let mut written = 0;
        let mut failed = false;
        while let Ok(n @ 1..) = pipe.read(&mut chunk).await {
            filter.clean(&chunk[..n], &mut text);
            if !failed {
                failed = append(&mut file, &mut written, &text.buf).is_err();
            }
            text.buf.clear();
        }
    };
    let _ = tokio::join!(log, child.wait());
    if let Some(jobs) = jobs.upgrade() {
        let mut pids = lock(&jobs.pids);
        // Not there when killed.
        if let Some(i) = pids.iter().position(|&p| Some(p) == pid) {
            pids.swap_remove(i);
            jobs.ended.store(true, Ordering::Release);
        }
    }
}

/// Append `data` to `file`, which holds `written` bytes, starting it again
/// when it would grow past [`MAX_BASH_LOG_BYTES`].
fn append(file: &mut File, written: &mut u64, data: &[u8]) -> io::Result<()> {
    const RESTARTED: &[u8] = b"[earlier output dropped]\n";
    if *written + data.len() as u64 > MAX_BASH_LOG_BYTES {
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(RESTARTED)?;
        *written = RESTARTED.len() as u64;
    }
    // A single chunk is far below the limit.
    file.write_all(data)?;
    *written += data.len() as u64;
    Ok(())
}

/// Kills the command's process group if a pid was recorded.
pub(super) fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}
