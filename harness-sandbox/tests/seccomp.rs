// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Unix sockets lead to programs of the host, which can reach the network.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;

use harness_sandbox::block_unix_sockets;

/// Set in the copy of the test process that blocks Unix sockets, which
/// cannot be undone.
const CHILD: &str = "HARNESS_TEST_SECCOMP_CHILD";

/// Run `body` in a copy of this test process with Unix sockets blocked.
fn blocked(name: &str, body: fn()) {
    if std::env::var_os(CHILD).is_some() {
        block_unix_sockets().unwrap();
        body();
        return;
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{name} failed with Unix sockets blocked");
}

#[test]
fn unix_sockets_are_refused() {
    blocked("unix_sockets_are_refused", || {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sock");
        let err = UnixListener::bind(&path).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied, "{err}");
        let err = UnixStream::connect(&path).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied, "{err}");
    });
}

#[test]
fn socket_pairs_still_work() {
    blocked("socket_pairs_still_work", || {
        use std::io::{Read, Write};
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(b"x").unwrap();
        let mut buf = [0];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"x");
    });
}

#[test]
fn other_sockets_still_work() {
    // The network namespace, not this filter, takes the network away.
    blocked("other_sockets_still_work", || {
        TcpListener::bind("127.0.0.1:0").unwrap();
    });
}

#[test]
fn io_uring_is_unavailable() {
    blocked("io_uring_is_unavailable", || {
        let mut params = [0u8; 120];
        // SAFETY: io_uring_setup with a zeroed parameter block.
        let res = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr()) };
        assert_eq!(res, -1);
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(errno, Some(libc::ENOSYS));
    });
}

/// Set in a program started by the copy with Unix sockets blocked.
const PROBE: &str = "HARNESS_TEST_SECCOMP_PROBE";

#[test]
fn commands_inherit_the_block() {
    blocked("commands_inherit_the_block", || {
        // A program started afterwards is refused too.
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "unix_socket_probe"])
            .env_remove(CHILD)
            .env(PROBE, "1")
            .status()
            .unwrap();
        assert!(status.success());
    });
}

/// Run by `commands_inherit_the_block`: Unix sockets must be refused.
#[test]
fn unix_socket_probe() {
    if std::env::var_os(PROBE).is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let err = UnixListener::bind(dir.path().join("app.sock")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied, "{err}");
}
