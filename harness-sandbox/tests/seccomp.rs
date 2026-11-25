// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The seccomp filter of the sandbox: no user namespaces, no mount
//! changes, and no Unix sockets, which lead to programs of the host that
//! can reach the network.

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
    run_child(
        Command::new(std::env::current_exe().unwrap()),
        name,
        network,
    );
}

/// Run test `name` with `cmd`, a copy of this test process or a program
/// starting one, restricted for `network` access.
fn run_child(mut cmd: Command, name: &str, network: &str) {
    let status = cmd
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

/// The error of creating a socket pair of `family` and `kind`, if any.
fn pair_error(family: libc::c_int, kind: libc::c_int) -> Option<i32> {
    let mut fds = [0; 2];
    // SAFETY: plain system calls on an array that outlives them.
    unsafe {
        if libc::socketpair(family, kind, 0, fds.as_mut_ptr()) != 0 {
            return std::io::Error::last_os_error().raw_os_error();
        }
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
    None
}

#[test]
fn datagram_socket_pairs_are_refused() {
    // Given an address, they send to any datagram socket of the host.
    blocked("datagram_socket_pairs_are_refused", check_pairs);
}

#[test]
fn datagram_socket_pairs_are_refused_with_only_ports() {
    restricted(
        "datagram_socket_pairs_are_refused_with_only_ports",
        "ports",
        check_pairs,
    );
}

fn check_pairs() {
    let flags = libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    for kind in [libc::SOCK_DGRAM, libc::SOCK_DGRAM | flags] {
        assert_eq!(pair_error(libc::AF_UNIX, kind), Some(libc::EACCES));
    }
    assert_eq!(pair_error(libc::AF_UNIX, libc::SOCK_SEQPACKET), None);
    assert_eq!(pair_error(libc::AF_UNIX, libc::SOCK_STREAM | flags), None);
}

#[test]
fn datagram_socket_pairs_stay_with_any_network() {
    restricted("datagram_socket_pairs_stay_with_any_network", "any", || {
        assert_eq!(pair_error(libc::AF_UNIX, libc::SOCK_DGRAM), None);
    });
}

#[test]
fn socket_pairs_are_unix_only() {
    // The kernel would look for a module for others, such as TIPC's.
    restricted("socket_pairs_are_unix_only", "any", || {
        for family in [libc::AF_INET, libc::AF_TIPC] {
            assert_eq!(pair_error(family, libc::SOCK_STREAM), Some(libc::EACCES));
        }
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
fn mounts_stay_even_as_root() {
    const NAME: &str = "mounts_stay_even_as_root";
    if std::env::var_os(CHILD).is_none() {
        // Root of a user namespace with a mount namespace of its own, which
        // needs no privilege.
        let mut cmd = Command::new("unshare");
        cmd.args(["--map-root-user", "--mount"])
            .arg(std::env::current_exe().unwrap());
        run_child(cmd, NAME, "none");
        return;
    }
    let tmp = c"/tmp";
    let tmpfs = c"tmpfs";
    // SAFETY: plain system calls on C strings that outlive them.
    let mount_tmpfs = || unsafe {
        libc::mount(
            tmpfs.as_ptr(),
            tmp.as_ptr(),
            tmpfs.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    // Over the /tmp of this mount namespace only.
    let res = mount_tmpfs();
    assert_eq!(res, 0, "{}", std::io::Error::last_os_error());
    restrict_syscalls(NetworkAccess::None).unwrap();
    assert_eq!(errno(mount_tmpfs().into()), Some(libc::EPERM));
    // SAFETY: as above.
    unsafe {
        let res = libc::umount2(tmp.as_ptr(), libc::MNT_DETACH);
        assert_eq!(errno(res.into()), Some(libc::EPERM));
        let flags = libc::MS_REMOUNT | libc::MS_BIND;
        let res = libc::mount(
            std::ptr::null(),
            tmp.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        );
        assert_eq!(errno(res.into()), Some(libc::EPERM));
        // A copy of the mount without the ones over it: OPEN_TREE_CLONE.
        let res = libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, tmp.as_ptr(), 1);
        assert_eq!(errno(res), Some(libc::EPERM));
        let res = libc::syscall(libc::SYS_fsopen, tmpfs.as_ptr(), 0);
        assert_eq!(errno(res), Some(libc::EPERM));
    }
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
    protocol_error(family, kind, 0)
}

/// Like [`socket_error`], for `protocol`.
fn protocol_error(family: libc::c_int, kind: libc::c_int, protocol: libc::c_int) -> Option<i32> {
    // SAFETY: plain system calls.
    unsafe {
        let fd = libc::socket(family, kind, protocol);
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

#[test]
fn usual_protocols_still_work() {
    blocked("usual_protocols_still_work", || {
        for family in [libc::AF_INET, libc::AF_INET6] {
            assert_eq!(
                protocol_error(family, libc::SOCK_STREAM, libc::IPPROTO_TCP),
                None
            );
            assert_eq!(
                protocol_error(family, libc::SOCK_DGRAM, libc::IPPROTO_UDP),
                None
            );
        }
        let route = libc::NETLINK_ROUTE;
        assert_eq!(
            protocol_error(libc::AF_NETLINK, libc::SOCK_RAW, route),
            None
        );
    });
}

#[test]
fn other_protocols_are_refused() {
    // The kernel would look for a module for them, such as SCTP's. Tested
    // with numbers no module provides, so that a broken filter fails the
    // test rather than load one.
    restricted("other_protocols_are_refused", "any", || {
        for family in [libc::AF_INET, libc::AF_INET6] {
            for kind in [libc::SOCK_STREAM, libc::SOCK_DGRAM] {
                assert_eq!(protocol_error(family, kind, 253), Some(libc::EACCES));
            }
        }
        assert_eq!(
            protocol_error(libc::AF_NETLINK, libc::SOCK_RAW, 31),
            Some(libc::EACCES)
        );
    });
}

#[test]
fn only_stream_and_datagram_ip_sockets() {
    // The default protocol of the other types is one more module to look
    // for, SCTP for `SOCK_SEQPACKET`, which the limit on ports misses.
    for network in ["none", "ports", "any"] {
        restricted("only_stream_and_datagram_ip_sockets", network, || {
            const SOCK_DCCP: libc::c_int = 6;
            let flags = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
            for family in [libc::AF_INET, libc::AF_INET6] {
                for kind in [libc::SOCK_SEQPACKET, SOCK_DCCP, libc::SOCK_RAW] {
                    assert_eq!(socket_error(family, kind), Some(libc::EACCES));
                    assert_eq!(socket_error(family, kind | flags), Some(libc::EACCES));
                }
            }
        });
    }
}

/// The errors of sending with `flags` through `sendto`, `sendmsg` and
/// `sendmmsg` on no socket: the filter answers first.
fn send_errors(flags: libc::c_int) -> [Option<i32>; 3] {
    // SAFETY: plain system calls with empty messages, failing on the file
    // descriptor at the latest.
    unsafe {
        let mut msg: libc::mmsghdr = std::mem::zeroed();
        [
            libc::sendto(-1, std::ptr::null(), 0, flags, std::ptr::null(), 0) as libc::c_long,
            libc::sendmsg(-1, &msg.msg_hdr, flags) as libc::c_long,
            libc::sendmmsg(-1, &mut msg, 1, flags as _) as libc::c_long,
        ]
        .map(errno)
    }
}

#[test]
fn fast_open_is_refused_with_only_ports() {
    // It connects without `connect`, the only call Landlock checks.
    restricted("fast_open_is_refused_with_only_ports", "ports", || {
        let fast_open = libc::MSG_FASTOPEN | libc::MSG_NOSIGNAL;
        assert_eq!(send_errors(fast_open), [Some(libc::EOPNOTSUPP); 3]);
        assert_eq!(send_errors(libc::MSG_NOSIGNAL), [Some(libc::EBADF); 3]);
    });
}

#[test]
fn fast_open_stays_with_any_network() {
    restricted("fast_open_stays_with_any_network", "any", || {
        assert_eq!(send_errors(libc::MSG_FASTOPEN), [Some(libc::EBADF); 3]);
    });
}

#[cfg(target_arch = "x86_64")]
#[test]
fn x32_system_calls_kill() {
    use std::os::unix::process::ExitStatusExt;

    const NAME: &str = "x32_system_calls_kill";
    // The x32 ABI shares the architecture value of x86_64: its system calls
    // could get around the filter.
    if std::env::var(CHILD).is_ok() {
        restrict_syscalls(NetworkAccess::None).unwrap();
        // SAFETY: getpid, through the x32 ABI, takes no argument.
        unsafe { libc::syscall(libc::SYS_getpid | 0x4000_0000) };
        return;
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture"])
        .env(CHILD, "none")
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(libc::SIGSYS));
}
