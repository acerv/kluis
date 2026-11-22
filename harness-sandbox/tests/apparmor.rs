// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use harness_sandbox::apparmor::{check_owner, profile, userns_blocked_in};
use tempfile::TempDir;

/// A fake `/proc` with the given restriction and labels, where present.
fn proc(restrict: Option<&str>, label: Option<&str>, old_label: Option<&str>) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("sys/kernel/apparmor_restrict_unprivileged_userns", restrict),
        ("self/attr/apparmor/current", label),
        ("self/attr/current", old_label),
    ];
    for (path, content) in files {
        if let Some(content) = content {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }
    dir
}

#[test]
fn blocked_when_restricted_and_unconfined() {
    let dir = proc(Some("1\n"), Some("unconfined\n"), None);
    assert!(userns_blocked_in(dir.path()));
}

#[test]
fn blocked_with_the_old_label_file() {
    let dir = proc(Some("1\n"), None, Some("unconfined\n"));
    assert!(userns_blocked_in(dir.path()));
}

#[test]
fn not_blocked_when_restriction_is_off_or_missing() {
    let off = proc(Some("0\n"), Some("unconfined\n"), None);
    assert!(!userns_blocked_in(off.path()));
    let missing = proc(None, Some("unconfined\n"), None);
    assert!(!userns_blocked_in(missing.path()));
}

#[test]
fn not_blocked_under_a_profile() {
    let dir = proc(Some("1\n"), Some("kl (enforce)\n"), None);
    assert!(!userns_blocked_in(dir.path()));
}

#[test]
fn owner_check_accepts_system_programs() {
    let sh = fs::canonicalize("/bin/sh").unwrap();
    // In a user namespace that does not map root, as in a sandbox, system
    // files belong to nobody.
    if fs::metadata(&sh).unwrap().uid() != 0 {
        return;
    }
    check_owner(&sh).unwrap();
}

#[test]
fn owner_check_refuses_user_files() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("kl");
    fs::write(&exe, "").unwrap();
    let err = check_owner(&exe).unwrap_err();
    assert!(err.message.contains("other than root"), "{}", err.message);
}

#[test]
fn owner_check_refuses_symlinks_and_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("sh");
    std::os::unix::fs::symlink("/bin/sh", &link).unwrap();
    assert!(check_owner(&link).is_err());
    assert!(check_owner(Path::new("bin/sh")).is_err());
}

#[test]
fn profile_attaches_to_the_path_and_drops_capabilities_of_children() {
    let text = profile("kl", Path::new("/opt/my kl/kl")).unwrap();
    assert!(
        text.contains(r#"profile kl "/opt/my kl/kl" flags="#),
        "{text}"
    );
    assert!(text.contains("allow pix /** -> &kl//&kl_unpriv,"), "{text}");

    let (main, unpriv) = text.split_once("profile kl_unpriv").unwrap();
    assert!(main.contains("allow capability,"), "{main}");
    assert!(main.contains("allow userns,"), "{main}");
    assert!(!unpriv.contains("allow capability,"), "{unpriv}");
    assert!(unpriv.contains("audit deny capability,"), "{unpriv}");
    assert!(unpriv.contains("allow pix /** -> &kl_unpriv,"), "{unpriv}");
}

#[test]
fn profile_refuses_paths_apparmor_could_misread() {
    for path in ["kl", "/opt/k*l", "/opt/{a,b}/kl", "/opt/\"kl", "/opt/k\nl"] {
        assert!(profile("kl", Path::new(path)).is_err(), "{path:?}");
    }
}
