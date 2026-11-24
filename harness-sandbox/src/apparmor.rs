// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Ubuntu's AppArmor restriction of unprivileged user namespaces.
//!
//! Since 23.10, Ubuntu lets a program without privileges create user
//! namespaces only when an AppArmor profile allows it. [`profile`] writes
//! one in the style of Ubuntu's own for bubblewrap: the harness may create
//! the namespaces of its container, while every program it starts loses all
//! capabilities, so the harness cannot be used to get around the
//! restriction.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nix::unistd::geteuid;

use crate::SandboxError;

/// Whether AppArmor keeps the calling process from creating the user
/// namespaces of a container: the restriction is on, and no profile of its
/// own allows them.
pub fn userns_blocked() -> bool {
    !geteuid().is_root() && userns_blocked_in(Path::new("/proc"))
}

/// Like [`userns_blocked`] for an unprivileged process, reading the files
/// of `proc` instead of `/proc`.
pub fn userns_blocked_in(proc: &Path) -> bool {
    let read = |path: &str| fs::read_to_string(proc.join(path)).ok();
    let restricted = read("sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_some_and(|value| value.trim() == "1");
    let label = read("self/attr/apparmor/current").or_else(|| read("self/attr/current"));
    restricted && label.is_some_and(|label| label.trim() == "unconfined")
}

/// Check that only root can change `exe` and the folders above it, so that
/// a profile attached to it cannot be handed to another program.
pub fn check_owner(exe: &Path) -> Result<(), SandboxError> {
    if !exe.is_absolute() {
        return Err(SandboxError::at(exe, "not an absolute path"));
    }
    for path in exe.ancestors() {
        let meta = fs::symlink_metadata(path).map_err(|e| SandboxError::at(path, e))?;
        if meta.file_type().is_symlink() {
            return Err(SandboxError::at(path, "is a symbolic link"));
        }
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(SandboxError::at(
                path,
                "can be changed by users other than root",
            ));
        }
    }
    Ok(())
}

/// The AppArmor profile `name` for the program at `exe`, which may create
/// user namespaces with capabilities in them, and the profile
/// `<name>_unpriv` stacked on every program it starts, which has no
/// capabilities.
///
/// Fails if `exe` is not absolute, or has characters other than letters,
/// digits, spaces and `/._+-`, which AppArmor could take for patterns.
pub fn profile(name: &str, exe: &Path) -> Result<String, SandboxError> {
    let path = exe
        .to_str()
        .filter(|path| {
            exe.is_absolute()
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || " /._+-".contains(c))
        })
        .ok_or_else(|| {
            SandboxError::at(
                exe,
                "AppArmor needs an absolute path of letters, digits, spaces and /._+-",
            )
        })?;
    Ok(format!(
        r#"# AppArmor profile for {path}
#
# The program may create the user namespaces of its sandbox. Every program
# it starts runs stacked with {name}_unpriv, which has no capabilities, so it
# cannot be used to get around the user namespace restriction.
#
# Remove with:  sudo apparmor_parser -R /etc/apparmor.d/{name}
#               sudo rm /etc/apparmor.d/{name}

abi <abi/4.0>,

include <tunables/global>

profile {name} "{path}" flags=(attach_disconnected,mediate_deleted) {{
  allow capability,
{RULES}
  # Stacked, as the sandbox sets no_new_privs.
  allow pix /** -> &{name}//&{name}_unpriv,

  include if exists <local/{name}>
}}

profile {name}_unpriv flags=(attach_disconnected,mediate_deleted) {{
{RULES}
  allow pix /** -> &{name}_unpriv,

  audit deny capability,

  include if exists <local/{name}_unpriv>
}}
"#
    ))
}

/// What both profiles allow, beside capabilities and running programs.
const RULES: &str = "  allow file rwlkm /{**,},
  allow network,
  allow unix,
  allow signal,
  allow ptrace,
  allow mqueue,
  allow userns,
  allow mount,
  allow umount,
  allow pivot_root,";
