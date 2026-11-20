// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A seccomp filter shrinking what commands can do to the kernel and the
//! host.

use std::io;

use nix::libc::{self, sock_filter, sock_fprog};

/// `seccomp_data` offsets: system call number, architecture, first argument
/// (its low half on these little-endian architectures).
const NR: u32 = 0;
const ARCH: u32 = 4;
const ARG0: u32 = 16;

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
const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;

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
/// Creating a socket fails with `EACCES` unless it is an IPv4, IPv6 or
/// netlink one, which a network namespace confines: others, such as vsock,
/// reach the host or the hypervisor past it. Unix ones are allowed only
/// with `unix_sockets`: a sandbox without network still shows sockets of
/// the host below the folders it shows, such as the ones of an editor or a
/// chat app, and the programs behind them can reach the network.
/// `socketpair` keeps working: its sockets only reach each other.
///
/// It cannot be undone, and applies to every thread of the process.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn restrict_syscalls(unix_sockets: bool) -> io::Result<()> {
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
        // `unshare` and `clone` take their flags first.
        jump_eq(libc::SYS_unshare as u32, 1, 0),
        jump_eq(libc::SYS_clone as u32, 0, 3),
        load(ARG0),
        jump_set(libc::CLONE_NEWUSER as u32, 0, 1),
        ret(EPERM),
    ]);
    let mut families = vec![libc::AF_INET, libc::AF_INET6, libc::AF_NETLINK];
    if unix_sockets {
        families.push(libc::AF_UNIX);
    }
    program.extend(socket_rules(&families));
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
pub fn restrict_syscalls(_unix_sockets: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "restricting system calls is not supported on this architecture",
    ))
}

/// Instructions allowing system calls other than `socket`, and sockets of
/// the address `families`, and refusing the others with `EACCES`.
fn socket_rules(families: &[libc::c_int]) -> Vec<sock_filter> {
    // Where each part lands: the call check, the family tests, then these.
    let deny = 3 + families.len();
    let allow = deny + 1;
    let skip = |from: usize, to: usize| (to - from - 1) as u8;

    let mut rules = vec![
        load(NR),
        jump_eq(libc::SYS_socket as u32, 0, skip(1, allow)),
        load(ARG0),
    ];
    for (i, &family) in families.iter().enumerate() {
        rules.push(jump_eq(family as u32, skip(3 + i, allow), 0));
    }
    rules.extend([ret(EACCES), ret(ALLOW)]);
    rules
}

fn load(offset: u32) -> sock_filter {
    statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset)
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
