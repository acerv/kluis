// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A seccomp filter shrinking what commands can do to the kernel and the
//! host.

use std::io;
use std::str::FromStr;

use nix::libc;
use seccompiler::{
    BackendError, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule,
};

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
/// connect instead. And it only checks `bind`, so `listen`, which binds a
/// socket not bound yet to a port of the host without it, fails with
/// `EOPNOTSUPP` too: with no port to bind, nothing could listen anyway.
///
/// A socket pair, also refused with `EACCES` unless Unix, is a datagram one
/// only with [`NetworkAccess::Any`]: given an address, such a socket sends
/// to any other. Stream and sequenced packet ones only reach each other, so
/// programs keep talking to the ones they start.
///
/// It cannot be undone, and applies to every thread of the process.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn restrict_syscalls(network: NetworkAccess) -> io::Result<()> {
    #[allow(unused_mut)]
    let mut programs = programs(network).map_err(io::Error::other)?;
    // The filters kill processes of another architecture, but not ones
    // using the x32 ABI, which shares the x86_64 one.
    #[cfg(target_arch = "x86_64")]
    programs[0].splice(0..0, kill_x32());
    // Applying one also sets no_new_privs, required to install it without
    // CAP_SYS_ADMIN, which also stops set-user-ID programs from gaining
    // privileges.
    for program in &programs {
        seccompiler::apply_filter_all_threads(program).map_err(io::Error::other)?;
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

type Fallible<T> = Result<T, BackendError>;

/// The rules of a filter: for each system call, either no rule, matching
/// any call, or rules of which one must match.
type Rules = Vec<(libc::c_long, Vec<SeccompRule>)>;

/// The filters for `network`. A filter takes one action on a match, so
/// there is one for each error; the kernel runs them all, and as their
/// system calls differ, at most one matches.
fn programs(network: NetworkAccess) -> Fallible<Vec<BpfProgram>> {
    let mut filters = vec![
        (
            libc::ENOSYS,
            vec![
                (libc::SYS_io_uring_setup, vec![]),
                (libc::SYS_clone3, vec![]),
            ],
        ),
        (libc::EPERM, privilege_rules()?),
        (libc::EACCES, socket_rules(network)?),
    ];
    if network == NetworkAccess::Ports {
        filters.push((libc::EOPNOTSUPP, ports_rules()?));
    }
    let arch = std::env::consts::ARCH.try_into()?;
    filters
        .into_iter()
        .map(|(errno, rules)| {
            let filter = SeccompFilter::new(
                rules.into_iter().collect(),
                SeccompAction::Allow,
                SeccompAction::Errno(errno as u32),
                arch,
            )?;
            BpfProgram::try_from(filter)
        })
        .collect()
}

/// Rules matching mount changes and new user namespaces.
fn privilege_rules() -> Fallible<Rules> {
    let mut rules: Rules = MOUNT_CALLS.iter().map(|&nr| (nr, vec![])).collect();
    // `unshare` and `clone` take their flags first.
    let new_user = libc::CLONE_NEWUSER as u64;
    for nr in [libc::SYS_unshare, libc::SYS_clone] {
        rules.push((nr, vec![rule([has(0, new_user)])?]));
    }
    Ok(rules)
}

/// Rules matching sockets of other address families than IPv4, IPv6,
/// netlink, and Unix ones with [`NetworkAccess::Any`], with another
/// protocol than the ones listed for their family, IPv4 and IPv6 ones
/// that are neither stream nor datagram sockets, and UDP ones with
/// [`NetworkAccess::Ports`]; and socket pairs other than Unix ones, or
/// datagram ones unless with [`NetworkAccess::Any`].
fn socket_rules(network: NetworkAccess) -> Fallible<Rules> {
    let mut families: Vec<(libc::c_int, &[libc::c_int])> = vec![
        (libc::AF_INET, INET_PROTOCOLS),
        (libc::AF_INET6, INET6_PROTOCOLS),
        (libc::AF_NETLINK, &[libc::NETLINK_ROUTE]),
    ];
    if network == NetworkAccess::Any {
        families.push((libc::AF_UNIX, &[]));
    }
    let mut socket = vec![rule(families.iter().map(|&(family, _)| ne(0, family)))?];
    for &(family, protocols) in families.iter().filter(|(_, p)| !p.is_empty()) {
        let protocols = protocols.iter().map(|&protocol| ne(2, protocol));
        socket.push(rule(std::iter::once(eq(0, family)).chain(protocols))?);
        if family == libc::AF_NETLINK {
            continue;
        }
        // The default protocol of the other types is not a listed one,
        // such as SCTP for `SOCK_SEQPACKET`.
        for kind in (0..=SOCK_TYPE_MASK).filter(|kind| !IP_TYPES.contains(kind)) {
            socket.push(rule([eq(0, family), of_type(kind)])?);
        }
        if network == NetworkAccess::Ports {
            socket.push(rule([eq(0, family), datagram()])?);
        }
    }
    let mut socket_pair = vec![rule([ne(0, libc::AF_UNIX)])?];
    if network != NetworkAccess::Any {
        socket_pair.push(rule([datagram()])?);
    }
    Ok(vec![
        (libc::SYS_socket, socket),
        (libc::SYS_socketpair, socket_pair),
    ])
}

/// Rules matching what goes past Landlock: sends with `MSG_FASTOPEN`, and
/// `listen`.
fn ports_rules() -> Fallible<Rules> {
    let fast_open = libc::MSG_FASTOPEN as u64;
    // `sendmsg` takes its flags third, `sendto` and `sendmmsg` fourth.
    let mut rules = [
        (libc::SYS_sendmsg, 2),
        (libc::SYS_sendto, 3),
        (libc::SYS_sendmmsg, 3),
    ]
    .into_iter()
    .map(|(nr, arg)| Ok((nr, vec![rule([has(arg, fast_open)])?])))
    .collect::<Fallible<Rules>>()?;
    rules.push((libc::SYS_listen, vec![]));
    Ok(rules)
}

/// A comparison of an argument, on its low 32 bits, with a value.
type Condition = (u8, SeccompCmpOp, u64);

fn eq(arg: u8, value: libc::c_int) -> Condition {
    (arg, SeccompCmpOp::Eq, value as u64)
}

fn ne(arg: u8, value: libc::c_int) -> Condition {
    (arg, SeccompCmpOp::Ne, value as u64)
}

/// Whether the argument has all the bits of `flags` set.
fn has(arg: u8, flags: u64) -> Condition {
    (arg, SeccompCmpOp::MaskedEq(flags), flags)
}

/// The bits of the type of a socket, without flags such as
/// `SOCK_NONBLOCK`.
const SOCK_TYPE_MASK: libc::c_int = 0xf;

/// The types IPv4 and IPv6 sockets may have.
const IP_TYPES: [libc::c_int; 2] = [libc::SOCK_STREAM, libc::SOCK_DGRAM];

/// Whether the type of a socket, without its flags, is `kind`.
fn of_type(kind: libc::c_int) -> Condition {
    let mask = SOCK_TYPE_MASK as u64;
    (1, SeccompCmpOp::MaskedEq(mask), kind as u64)
}

/// Whether the type of a socket is a datagram one.
fn datagram() -> Condition {
    of_type(libc::SOCK_DGRAM)
}

/// A rule matching when all `conditions` hold.
fn rule(conditions: impl IntoIterator<Item = Condition>) -> Fallible<SeccompRule> {
    let conditions = conditions
        .into_iter()
        .map(|(arg, op, value)| SeccompCondition::new(arg, SeccompCmpArgLen::Dword, op, value))
        .collect::<Fallible<_>>()?;
    SeccompRule::new(conditions)
}

/// Instructions killing the process on a system call of the x32 ABI, then
/// going on.
#[cfg(target_arch = "x86_64")]
fn kill_x32() -> [seccompiler::sock_filter; 3] {
    const X32_BIT: u32 = 0x4000_0000;
    let op = |code: u32, jt, jf, k| seccompiler::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    };
    [
        // The system call number in `seccomp_data`.
        op(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
        op(libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K, 0, 1, X32_BIT),
        op(
            libc::BPF_RET | libc::BPF_K,
            0,
            0,
            libc::SECCOMP_RET_KILL_PROCESS,
        ),
    ]
}
