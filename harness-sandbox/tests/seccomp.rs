// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The seccomp filter of the sandbox: no user namespaces, and no Unix
//! sockets, which lead to programs of the host that can reach the network.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;

use harness_sandbox::{restrict_syscalls, NetworkAccess};

/// Set in the copy of the test process that restricts its system calls,
/// which cannot be undone, to its network access.
const CHILD: &str = "HARNESS_TEST_SECCOMP_CHILD";

/// Run `body` in a copy of this test process restricted as with no network.
fn blocked(name: &str, body: fn()) {
    restricted(name, "none", body);
}

/// Run `body` in a copy of this test process with its system calls
/// restricted for `network` access (none, ports or any).
fn restricted(name: &str, network: &str, body: fn()) {
    if let Ok(network) = std::env::var(CHILD) {
        let network: NetworkAccess = network.parse().unwrap();
        restrict_syscalls(network).unwrap();
        body();
        return;
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, network)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "{name} failed with system calls restricted"
    );
}

/// The error of a failed raw system call.
fn errno(res: libc::c_long) -> Option<i32> {
    assert_eq!(res, -1);
    std::io::Error::last_os_error().raw_os_error()
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
        assert_eq!(errno(res), Some(libc::ENOSYS));
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

#[test]
fn unix_sockets_can_stay_allowed() {
    restricted("unix_sockets_can_stay_allowed", "any", || {
        let dir = tempfile::tempdir().unwrap();
        UnixListener::bind(dir.path().join("app.sock")).unwrap();
    });
}

#[test]
fn user_namespaces_are_refused() {
    // With any network too: it does not matter here.
    restricted("user_namespaces_are_refused", "any", || {
        // SAFETY: plain system calls; a clone that succeeded would leave
        // its child at once.
        unsafe {
            let res = libc::syscall(libc::SYS_unshare, libc::CLONE_NEWUSER);
            assert_eq!(errno(res), Some(libc::EPERM));
            let flags = libc::CLONE_NEWUSER | libc::SIGCHLD;
            let res = libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0);
            if res == 0 {
                libc::_exit(0);
            }
            assert_eq!(errno(res), Some(libc::EPERM));
            // Its flags cannot be checked, so it is not available at all.
            let res = libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0);
            assert_eq!(errno(res), Some(libc::ENOSYS));
        }
    });
}

#[test]
fn threads_and_programs_still_start() {
    // Without clone3, they fall back to clone.
    restricted("threads_and_programs_still_start", "none", || {
        std::thread::spawn(|| ()).join().unwrap();
        let status = Command::new("true").status().unwrap();
        assert!(status.success());
    });
}

/// The error of creating a socket of `family` and `kind`, if any.
fn socket_error(family: libc::c_int, kind: libc::c_int) -> Option<i32> {
    // SAFETY: plain system calls.
    unsafe {
        let fd = libc::socket(family, kind, 0);
        if fd < 0 {
            return std::io::Error::last_os_error().raw_os_error();
        }
        libc::close(fd);
    }
    None
}

#[test]
fn only_ip_and_netlink_sockets() {
    // vsock and the like are not confined by the network namespace.
    blocked("only_ip_and_netlink_sockets", check_families);
}

#[test]
fn unix_sockets_do_not_open_other_families() {
    restricted(
        "unix_sockets_do_not_open_other_families",
        "any",
        check_families,
    );
}

/// Address families that get past a network namespace or that commands do
/// not need: vsock, kernel crypto, PPPoX, KCM, SMC and MCTP.
const REFUSED: [libc::c_int; 6] = [libc::AF_VSOCK, libc::AF_ALG, libc::AF_PPPOX, 41, 43, 45];

fn check_families() {
    for family in REFUSED {
        assert_eq!(
            socket_error(family, libc::SOCK_STREAM),
            Some(libc::EACCES),
            "{family}"
        );
    }
    assert_eq!(socket_error(libc::AF_INET, libc::SOCK_STREAM), None);
    assert_eq!(socket_error(libc::AF_INET6, libc::SOCK_STREAM), None);
    assert_eq!(socket_error(libc::AF_NETLINK, libc::SOCK_RAW), None);
}

#[test]
fn udp_is_refused_with_only_ports() {
    // Landlock only limits TCP.
    restricted("udp_is_refused_with_only_ports", "ports", || {
        let flags = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
        for family in [libc::AF_INET, libc::AF_INET6] {
            assert_eq!(socket_error(family, libc::SOCK_DGRAM), Some(libc::EACCES));
            assert_eq!(
                socket_error(family, libc::SOCK_DGRAM | flags),
                Some(libc::EACCES)
            );
            assert_eq!(socket_error(family, libc::SOCK_STREAM | flags), None);
        }
        assert_eq!(socket_error(libc::AF_NETLINK, libc::SOCK_DGRAM), None);
        assert_eq!(
            socket_error(libc::AF_UNIX, libc::SOCK_STREAM),
            Some(libc::EACCES)
        );
    });
}

#[test]
fn udp_stays_without_network() {
    // The empty network namespace confines it.
    blocked("udp_stays_without_network", || {
        assert_eq!(socket_error(libc::AF_INET, libc::SOCK_DGRAM), None);
    });
}

#[test]
fn udp_stays_with_any_network() {
    restricted("udp_stays_with_any_network", "any", || {
        assert_eq!(socket_error(libc::AF_INET6, libc::SOCK_DGRAM), None);
    });
}
