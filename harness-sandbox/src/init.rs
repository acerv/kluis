// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! An init for the container, and stopping what commands leave running.

use std::io;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{close, fork, getpid, ForkResult, Pid};

/// How long [`stop_others`] waits for the processes to be gone.
const STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// In a PID namespace where the calling process is the init, fork: the
/// parent stays the init and reaps the processes commands leave behind,
/// which the kernel hands to it, while the child returns and goes on.
/// Elsewhere, does nothing.
///
/// Returns whether the caller now runs below such an init, where
/// [`stop_others`] may be used. Call it before starting any thread.
pub fn start_init() -> io::Result<bool> {
    if getpid().as_raw() != 1 {
        return Ok(false);
    }
    // SAFETY: no other thread runs yet, so the child may run any code.
    match unsafe { fork() }? {
        ForkResult::Child => Ok(true),
        ForkResult::Parent { child } => reap(child),
    }
}

/// Reap every process until `child` exits, then exit as it did.
fn reap(child: Pid) -> ! {
    // Only the child talks to the host: without copies of its pipes here,
    // the host sees at once when it is gone.
    let _ = close(0);
    let _ = close(1);
    loop {
        match waitpid(None, None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == child => std::process::exit(code),
            Ok(WaitStatus::Signaled(pid, signal, _)) if pid == child => {
                std::process::exit(128 + signal as i32)
            }
            Ok(_) | Err(Errno::EINTR) => {}
            Err(_) => std::process::exit(1),
        }
    }
}

/// Kill every process of the namespace but the init, the caller and the
/// sessions in `keep`, such as programs commands left running in the
/// background, and wait until they are gone. Only below the init of
/// [`start_init`].
pub fn stop_others(keep: &[u32]) {
    let start = std::time::Instant::now();
    // Each pass kills what the last one missed, such as a process forked
    // while it ran.
    while kill_others(keep) && start.elapsed() < STOP_TIMEOUT {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Kill every process but the init, the caller and the sessions in `keep`,
/// and return whether `/proc` listed any.
fn kill_others(keep: &[u32]) -> bool {
    let me = getpid().as_raw();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    let mut found = false;
    for pid in dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok()) {
        if pid == 1 || pid == me {
            continue;
        }
        // A zombie is already gone, only not reaped yet.
        match stat(pid) {
            Some((state, sid)) if state == 'Z' || keep.contains(&sid) => continue,
            _ => {}
        }
        found = true;
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    found
}

/// The state and the session of process `pid`.
fn stat(pid: i32) -> Option<(char, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the command name, which may hold anything: state, parent,
    // group, session.
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let sid = fields.nth(2)?.parse().ok()?;
    Some((state, sid))
}
