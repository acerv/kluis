// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Kept apart from other tests: protecting the test process applies to all
//! its threads.

use std::process::{Command, Output};

use harness_sandbox::protect_self;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn children_cannot_read_environment() {
    protect_self().unwrap();

    // Another process: this one can always read its own.
    let out = Command::new("cat")
        .arg(format!("/proc/{}/environ", std::process::id()))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
}
