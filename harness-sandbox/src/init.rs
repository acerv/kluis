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

/// Kill every process of the namespace but the init and the caller, such
/// as programs commands left running in the background, and wait until
/// they are gone. Only below the init of [`start_init`].
pub fn stop_others() {
    let _ = kill(Pid::from_raw(-1), Signal::SIGKILL);
    let start = std::time::Instant::now();
    while others_left() && start.elapsed() < STOP_TIMEOUT {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Whether `/proc` still lists a process but the init and the caller.
fn others_left() -> bool {
    let me = getpid().as_raw();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .any(|pid| pid != 1 && pid != me)
}
