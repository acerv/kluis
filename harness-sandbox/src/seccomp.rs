// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A seccomp filter shrinking what commands can do to the kernel and the
//! host.

use std::io;
use std::str::FromStr;

use nix::libc::{self, sock_filter, sock_fprog};

/// `seccomp_data` offsets: system call number, architecture, arguments
/// (their low half on these little-endian architectures).
const NR: u32 = 0;
const ARCH: u32 = 4;
const ARG0: u32 = 16;
const ARG1: u32 = 24;
const ARG2: u32 = 32;
const ARG3: u32 = 40;

#[cfg(target_arch = "x86_64")]
const NATIVE_ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
#[cfg(target_arch = "aarch64")]
const NATIVE_ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64

/// System calls of the x32 ABI, which share the x86_64 architecture value.
#[cfg(target_arch = "x86_64")]
const X32_BIT: u32 = 0x4000_0000;

const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const EACCES: u32 = libc::SECCOMP_RET_ERRNO | libc::EACCES as u32;
const EPERM: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
const ENOSYS: u32 = libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32;
const EOPNOTSUPP: u32 = libc::SECCOMP_RET_ERRNO | libc::EOPNOTSUPP as u32;
const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;

/// The network commands get, which decides the sockets they may create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAccess {
    /// None: the network namespace is empty.
    None,
    /// TCP to some ports, which Landlock enforces.
    Ports,
    /// Any network.
    Any,
}

impl FromStr for NetworkAccess {
    type Err = String;

    /// `none`, `ports` or `any`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "ports" => Ok(Self::Ports),
            "any" => Ok(Self::Any),
            _ => Err(format!(
                "invalid network access '{s}' (expected none, ports or any)"
            )),
        }
    }
}

/// Restrict the system calls of this process and the ones it starts.
///
/// Creating a user namespace fails with `EPERM`: root inside one cannot
/// undo the sandbox, but it reaches kernel code, such as the firewall,
/// that most kernel exploits go through. `clone3`, whose flags the filter
/// cannot read, and io_uring, as large a part of the kernel, fail with
/// `ENOSYS`, so programs fall back to `clone` and plain system calls. A
/// process making system calls of another architecture or ABI, which could
/// get around the filter, is killed.
///
/// Changing mounts fails with `EPERM`, as it does without privileges:
/// commands run as root, by mistake or through a bug, could otherwise
/// unmount the masks over denied paths.
///
/// Creating a socket fails with `EACCES` unless it is an IPv4, IPv6 or
/// netlink one, which a network namespace confines: others, such as vsock,
/// reach the host or the hypervisor past it. Those must use a common
/// protocol, TCP, UDP or ping, or netlink routing: for any other the kernel
/// would look for a module, loading rarely used code, such as SCTP, that
/// kernel exploits go through. Unix ones are allowed only
/// with [`NetworkAccess::Any`]: a sandbox without network still shows
/// sockets of the host below the folders it shows, such as the ones of an
/// editor or a chat app, and the programs behind them can reach the
/// network. With [`NetworkAccess::Ports`], UDP ones are refused too, as
/// Landlock only limits TCP. It also only checks `connect`, so sending
/// with `MSG_FASTOPEN`, which connects without it, fails with
/// `EOPNOTSUPP`, as when the host does not use TCP Fast Open: programs
/// connect instead.
///
/// A socket pair, also refused with `EACCES` unless Unix, is a datagram one
/// only with [`NetworkAccess::Any`]: given an address, such a socket sends
/// to any other. Stream and sequenced packet ones only reach each other, so
/// programs keep talking to the ones they start.
///
/// It cannot be undone, and applies to every thread of the process.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn restrict_syscalls(network: NetworkAccess) -> io::Result<()> {
    let mut program = vec![
        load(ARCH),
        // Another architecture: it could get around the filter.
        jump_eq(NATIVE_ARCH, 1, 0),
        ret(KILL),
        load(NR),
    ];
    #[cfg(target_arch = "x86_64")]
    program.extend([jump_ge(X32_BIT, 0, 1), ret(KILL)]);
    program.extend([
        jump_eq(libc::SYS_io_uring_setup as u32, 0, 1),
        ret(ENOSYS),
        jump_eq(libc::SYS_clone3 as u32, 0, 1),
        ret(ENOSYS),
    ]);
    for &nr in MOUNT_CALLS {
        program.extend([jump_eq(nr as u32, 0, 1), ret(EPERM)]);
    }
    program.extend([
        // `unshare` and `clone` take their flags first.
        jump_eq(libc::SYS_unshare as u32, 1, 0),
        jump_eq(libc::SYS_clone as u32, 0, 3),
        load(ARG0),
        jump_set(libc::CLONE_NEWUSER as u32, 0, 1),
        ret(EPERM),
    ]);
    let mut families: Vec<(libc::c_int, &[libc::c_int])> = vec![
        (libc::AF_INET, INET_PROTOCOLS),
        (libc::AF_INET6, INET6_PROTOCOLS),
        (libc::AF_NETLINK, &[libc::NETLINK_ROUTE]),
    ];
    if network == NetworkAccess::Any {
        families.push((libc::AF_UNIX, &[]));
    }
    program.extend(socket_pair_rules(network == NetworkAccess::Any));
    if network == NetworkAccess::Ports {
        program.extend(fast_open_rules());
    }
    program.extend(socket_rules(&families, network != NetworkAccess::Ports));
    let filter = sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr(),
    };

    // SAFETY: plain system calls; `filter` points into `program`, which
    // outlives them, and the kernel copies it.
    unsafe {
        // Required to install a filter without CAP_SYS_ADMIN; it also stops
        // set-user-ID programs from gaining privileges.
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &filter as *const sock_fprog,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// System calls cannot be restricted on this architecture: refuse to run
/// rather than leave them open.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn restrict_syscalls(_network: NetworkAccess) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "restricting system calls is not supported on this architecture",
    ))
}

/// `open_tree_attr`, which the libc crate does not name yet: the same
/// number on every architecture, as for any system call since 424.
const SYS_OPEN_TREE_ATTR: libc::c_long = 467;

/// System calls changing mounts, or getting a copy of one without the
/// mounts over it, which would undo the masks over denied paths.
const MOUNT_CALLS: &[libc::c_long] = &[
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    SYS_OPEN_TREE_ATTR,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_mount_setattr,
];

/// IPv4 protocols commands may use: the default for the type, TCP, UDP
/// and ping.
const INET_PROTOCOLS: &[libc::c_int] =
    &[0, libc::IPPROTO_TCP, libc::IPPROTO_UDP, libc::IPPROTO_ICMP];

/// Like [`INET_PROTOCOLS`], for IPv6.
const INET6_PROTOCOLS: &[libc::c_int] = &[
    0,
    libc::IPPROTO_TCP,
    libc::IPPROTO_UDP,
    libc::IPPROTO_ICMPV6,
];

/// Instructions refusing socket pairs other than Unix ones with `EACCES`,
/// and datagram ones unless `datagrams`.
fn socket_pair_rules(datagrams: bool) -> Vec<sock_filter> {
    let unix = libc::AF_UNIX as u32;
    let mut rules = vec![load(NR)];
    if datagrams {
        rules.extend([
            jump_eq(libc::SYS_socketpair as u32, 0, 3),
            load(ARG0),
            jump_eq(unix, 1, 0),
        ]);
    } else {
        rules.extend([
            jump_eq(libc::SYS_socketpair as u32, 0, 6),
            load(ARG0),
            jump_eq(unix, 0, 3),
            // The type, without flags such as SOCK_CLOEXEC.
            load(ARG1),
            and(0xf),
            jump_eq(libc::SOCK_DGRAM as u32, 0, 1),
        ]);
    }
    rules.push(ret(EACCES));
    rules
}

/// Instructions refusing sends with `MSG_FASTOPEN` with `EOPNOTSUPP`.
fn fast_open_rules() -> Vec<sock_filter> {
    let fast_open = libc::MSG_FASTOPEN as u32;
    vec![
        load(NR),
        // `sendmsg` takes its flags third, `sendto` and `sendmmsg` fourth.
        jump_eq(libc::SYS_sendmsg as u32, 0, 2),
        load(ARG2),
        jump_set(fast_open, 4, 5),
        jump_eq(libc::SYS_sendto as u32, 1, 0),
        jump_eq(libc::SYS_sendmmsg as u32, 0, 3),
        load(ARG3),
        jump_set(fast_open, 0, 1),
        ret(EOPNOTSUPP),
    ]
}

/// An instruction of [`socket_rules`], whose jump, when taken, may lead to
/// the final approval instead of the next instruction.
struct Op {
    op: sock_filter,
    allows: bool,
}

/// Instructions allowing system calls other than `socket`, and sockets of
/// the address `families` with one of their protocols, or any when there
/// are none listed, IP ones only for TCP unless `udp`, and refusing the
/// others with `EACCES`.
fn socket_rules(families: &[(libc::c_int, &[libc::c_int])], udp: bool) -> Vec<sock_filter> {
    let next = |op| Op { op, allows: false };
    // Classic BPF only jumps forward: the family tests and their refusal
    // come first, then a block per family listing protocols, which ends with
    // its own refusal, and the approval last.
    let blocks: Vec<Vec<Op>> = families
        .iter()
        .map(|&(family, protocols)| {
            let mut block = Vec::new();
            if protocols.is_empty() {
                return block;
            }
            if !udp && matches!(family, libc::AF_INET | libc::AF_INET6) {
                // The type, without flags such as SOCK_NONBLOCK.
                block.extend(
                    [
                        load(ARG1),
                        and(0xf),
                        jump_eq(libc::SOCK_DGRAM as u32, 0, 1),
                        ret(EACCES),
                    ]
                    .map(next),
                );
            }
            block.push(next(load(ARG2)));
            block.extend(protocols.iter().map(|&protocol| Op {
                op: jump_eq(protocol as u32, 0, 0),
                allows: true,
            }));
            block.push(next(ret(EACCES)));
            block
        })
        .collect();

    let tests = 3;
    let mut starts = Vec::with_capacity(blocks.len());
    let mut allow = tests + families.len() + 1;
    for block in &blocks {
        starts.push(allow);
        allow += block.len();
    }
    let skip = |from: usize, to: usize| u8::try_from(to - from - 1).expect("jump too long");

    let mut rules = vec![
        load(NR),
        jump_eq(libc::SYS_socket as u32, 0, skip(1, allow)),
        load(ARG0),
    ];
    for (i, &(family, _)) in families.iter().enumerate() {
        let to = if blocks[i].is_empty() {
            allow
        } else {
            starts[i]
        };
        rules.push(jump_eq(family as u32, skip(tests + i, to), 0));
    }
    rules.push(ret(EACCES));
    for Op { mut op, allows } in blocks.into_iter().flatten() {
        if allows {
            op.jt = skip(rules.len(), allow);
        }
        rules.push(op);
    }
    rules.push(ret(ALLOW));
    rules
}

fn load(offset: u32) -> sock_filter {
    statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset)
}

fn and(k: u32) -> sock_filter {
    statement(libc::BPF_ALU | libc::BPF_AND | libc::BPF_K, k)
}

fn ret(action: u32) -> sock_filter {
    statement(libc::BPF_RET | libc::BPF_K, action)
}

fn statement(code: u32, k: u32) -> sock_filter {
    sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}

/// Skip `then` instructions when the loaded value equals `k`, `otherwise`
/// instructions when not.
fn jump_eq(k: u32, then: u8, otherwise: u8) -> sock_filter {
    jump(libc::BPF_JEQ, k, then, otherwise)
}

/// Like [`jump_eq`], comparing with `>=`.
#[cfg(target_arch = "x86_64")]
fn jump_ge(k: u32, then: u8, otherwise: u8) -> sock_filter {
    jump(libc::BPF_JGE, k, then, otherwise)
}

/// Like [`jump_eq`], testing whether any bit of `k` is set.
fn jump_set(k: u32, then: u8, otherwise: u8) -> sock_filter {
    jump(libc::BPF_JSET, k, then, otherwise)
}

fn jump(op: u32, k: u32, then: u8, otherwise: u8) -> sock_filter {
    sock_filter {
        code: (libc::BPF_JMP | op | libc::BPF_K) as u16,
        jt: then,
        jf: otherwise,
        k,
    }
}
