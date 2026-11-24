// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command};

use common::{inside, system};
use harness_sandbox::{start_init, stop_others};

/// Start `sleep` in a session of its own.
fn sleeper() -> io::Result<Child> {
    let mut cmd = Command::new("sleep");
    cmd.arg("30");
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| nix::unistd::setsid().map(drop).map_err(io::Error::from));
    }
    cmd.spawn()
}

#[test]
fn stop_others_keeps_the_sessions_asked() {
    let out = inside(&system(), || {
        if !start_init()? {
            return Ok("no init".into());
        }
        let mut kept = sleeper()?;
        let mut other = sleeper()?;
        stop_others(&[kept.id()]);
        let other = other.wait()?.signal();
        let kept_running = kept.try_wait()?.is_none();
        kept.kill()?;
        Ok(format!("other={other:?} kept_running={kept_running}"))
    });
    assert_eq!(out.unwrap(), "other=Some(9) kept_running=true");
}
