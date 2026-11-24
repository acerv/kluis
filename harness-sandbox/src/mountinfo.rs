// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The mounts of the host on a filesystem that could reach the network.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use crate::SandboxError;

/// The mount points of `mountinfo` on a filesystem that could reach the
/// network, below one of the `roots` (a mount path and its canonical
/// path), as they show in the container: the sandbox hides them.
///
/// Mounts at or above an `allowed` path stay, as do mounts below another
/// returned one, which hiding already covers.
pub(crate) fn remote_points(
    roots: &[(&Path, &Path)],
    allowed: &[&Path],
    mountinfo: &str,
) -> Vec<PathBuf> {
    // Only the last mount on a path shows: it covers the earlier ones,
    // e.g. a disk mounted over its automount point.
    let top: HashMap<&str, &str> = mountinfo.lines().filter_map(mount_point_and_type).collect();
    let mut found = Vec::new();
    for (point, _) in top.into_iter().filter(|(_, fstype)| !is_local(fstype)) {
        let point = PathBuf::from(unescape(point));
        for (mount, canon) in roots {
            let Some(rest) = point
                .strip_prefix(canon)
                .ok()
                .filter(|r| !r.as_os_str().is_empty())
            else {
                continue;
            };
            let shown = mount.join(rest);
            if !allowed.iter().any(|path| path.starts_with(&shown)) {
                found.push(shown);
            }
        }
    }
    // Sorted, a path comes right before the ones below it.
    found.sort();
    found.dedup_by(|below, above| below.starts_with(above));
    found
}

/// The type of the filesystem holding `path` when it could reach the
/// network, e.g. FUSE or NFS: what commands write there may leave the
/// machine even with no network, as the sandbox shows `path` itself.
pub fn remote_filesystem(path: &Path) -> Option<String> {
    // A dead mount cannot be resolved: take the path as given then.
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mountinfo = read_mountinfo().ok()?;
    let mut holder = None;
    let mut depth = 0;
    // The deepest mount point above `path`, the last one mounted there.
    for (point, fstype) in mountinfo.lines().filter_map(mount_point_and_type) {
        let point = PathBuf::from(unescape(point));
        let point_depth = point.components().count();
        if path.starts_with(&point) && point_depth >= depth {
            holder = Some(fstype);
            depth = point_depth;
        }
    }
    holder.filter(|fstype| !is_local(fstype)).map(str::to_owned)
}

/// The mount point and filesystem type of a `/proc/self/mountinfo` line.
fn mount_point_and_type(line: &str) -> Option<(&str, &str)> {
    // Optional fields end with a lone "-"; spaces in paths are escaped.
    let (before, after) = line.split_once(" - ")?;
    let point = before.split(' ').nth(4)?;
    let fstype = after.split(' ').next()?;
    Some((point, fstype))
}

/// Whether the kernel serves a filesystem of type `fstype` from a local
/// device or from memory, so it cannot reach the network. Any other one,
/// FUSE, NFS, SMB, 9p, autofs or unknown, could.
fn is_local(fstype: &str) -> bool {
    matches!(
        fstype,
        // Disks.
        "ext2" | "ext3" | "ext4" | "btrfs" | "xfs" | "f2fs" | "zfs" | "bcachefs" | "jfs"
            | "reiserfs" | "nilfs2" | "vfat" | "msdos" | "exfat" | "ntfs3" | "hfsplus"
            | "iso9660" | "udf" | "squashfs" | "erofs" | "overlay"
            // Memory and the kernel itself.
            | "tmpfs" | "ramfs" | "proc" | "sysfs" | "devtmpfs" | "devpts" | "mqueue"
            | "cgroup" | "cgroup2" | "securityfs" | "debugfs" | "tracefs" | "pstore"
            | "bpf" | "efivarfs" | "configfs" | "hugetlbfs" | "binfmt_misc" | "selinuxfs"
            | "fusectl" | "nsfs"
    )
}

/// One path of `/proc/self/mountinfo`, where a backslash escapes the octal
/// value of a space, tab, newline or backslash.
fn unescape(field: &str) -> OsString {
    let mut out = Vec::with_capacity(field.len());
    let mut rest = field.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        match tail {
            [a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', after @ ..] if byte == b'\\' => {
                out.push((a - b'0') << 6 | (b - b'0') << 3 | (c - b'0'));
                rest = after;
            }
            _ => {
                out.push(byte);
                rest = tail;
            }
        }
    }
    OsString::from_vec(out)
}

/// The mount table of this namespace.
pub(crate) fn read_mountinfo() -> Result<String, SandboxError> {
    let path = Path::new("/proc/self/mountinfo");
    std::fs::read_to_string(path).map_err(|e| SandboxError::at(path, e))
}
