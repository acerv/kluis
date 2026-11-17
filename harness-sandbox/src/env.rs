// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Environment of the commands a harness runs.

use std::ffi::OsString;

/// Passed to commands: what programs need to find themselves, the user, the
/// language and the terminal. Not the temporary folders, which may not
/// exist in a container.
const COMMAND_ENV: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM"];

/// Variables of `vars` that commands may see: the [`COMMAND_ENV`] ones,
/// `LC_*` and the names in `pass`. Anything else, such as API keys, is
/// dropped.
pub fn command_env<I>(vars: I, pass: &[String]) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    vars.into_iter()
        .filter(|(name, _)| {
            name.to_str().is_some_and(|name| {
                COMMAND_ENV.contains(&name)
                    || name.starts_with("LC_")
                    || pass.iter().any(|p| p == name)
            })
        })
        .collect()
}
