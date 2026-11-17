// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::ffi::OsString;

use harness_sandbox::command_env;

fn vars(names: &[&str]) -> Vec<(OsString, OsString)> {
    names.iter().map(|n| (n.into(), "x".into())).collect()
}

fn names(env: &[(OsString, OsString)]) -> Vec<&str> {
    env.iter().map(|(k, _)| k.to_str().unwrap()).collect()
}

#[test]
fn keeps_only_allowed_names() {
    let env = command_env(
        vars(&[
            "PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "TERM",
            "DEEPSEEK_API_KEY",
            "PGPASSWORD",
            "TMPDIR",
            "SSH_AUTH_SOCK",
        ]),
        &[],
    );
    assert_eq!(names(&env), ["PATH", "HOME", "LANG", "LC_ALL", "TERM"]);
}

#[test]
fn passes_requested_names() {
    let env = command_env(
        vars(&["PATH", "CARGO_HOME", "OTHER"]),
        &["CARGO_HOME".into()],
    );
    assert_eq!(names(&env), ["PATH", "CARGO_HOME"]);
}

#[test]
fn keeps_values() {
    let path = (OsString::from("PATH"), OsString::from("/usr/bin"));
    assert_eq!(command_env([path.clone()], &[]), [path]);
}
