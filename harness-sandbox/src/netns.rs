// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! A way into a container without network, for a proxy on the host.

use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use nix::errno::Errno;
use nix::libc;
use nix::sched::{setns, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, ForkResult};
use rustix::net::{
    recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};

/// A TCP listener on `127.0.0.1:port` in the network namespace of the
/// process `pid`, such as a container started with
/// [`Sandbox::no_network`](crate::Sandbox::no_network), whose loopback
/// interface it brings up. Programs there connecting to it reach whoever
/// accepts on the host, and nothing else.
///
/// The namespace must belong to a user namespace this user created.
pub fn listen_in(pid: u32, port: u16) -> io::Result<TcpListener> {
    let namespace = |kind| File::open(format!("/proc/{pid}/ns/{kind}"));
    let (user, net) = (namespace("user")?, namespace("net")?);
    let (ours, theirs) = UnixStream::pair()?;
    // SAFETY: the child only makes system calls, with no allocation, and
    // exits without unwinding.
    match unsafe { fork() }? {
        ForkResult::Child => {
            let res = listen_inside(&user, &net, port).and_then(|l| send_fd(&theirs, l.as_fd()));
            // SAFETY: ends the child at once, as it must after `fork`.
            unsafe { libc::_exit(res.err().map_or(0, |e| e.raw_os_error().unwrap_or(1))) }
        }
        ForkResult::Parent { child } => {
            drop(theirs);
            let fd = recv_fd(&ours);
            let status = loop {
                match waitpid(child, None) {
                    Err(Errno::EINTR) => {}
                    status => break status?,
                }
            };
            match status {
                WaitStatus::Exited(_, 0) => fd.map(TcpListener::from),
                WaitStatus::Exited(_, errno) => Err(io::Error::from_raw_os_error(errno)),
                status => Err(io::Error::other(format!("{status:?}"))),
            }
        }
    }
}

/// In the namespaces `user` and `net`, bring up the loopback interface and
/// listen on `127.0.0.1:port`. Only in a process of its own: it cannot
/// leave them.
fn listen_inside(user: &File, net: &File, port: u16) -> io::Result<TcpListener> {
    setns(user, CloneFlags::CLONE_NEWUSER)?;
    setns(net, CloneFlags::CLONE_NEWNET)?;
    loopback_up()?;
    TcpListener::bind((Ipv4Addr::LOCALHOST, port))
}

/// Bring up the loopback interface, down in a new network namespace.
fn loopback_up() -> io::Result<()> {
    let check = |res: libc::c_int| match res {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    };
    // SAFETY: plain system calls on a zeroed `ifreq` and a socket closed
    // right after.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        check(fd)?;
        let mut req: libc::ifreq = std::mem::zeroed();
        req.ifr_name[..2].copy_from_slice(&[b'l' as libc::c_char, b'o' as libc::c_char]);
        let res = check(libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut req)).and_then(|()| {
            req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            check(libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &req))
        });
        libc::close(fd);
        res
    }
}

/// Send `fd` over `socket`.
fn send_fd(socket: &UnixStream, fd: BorrowedFd) -> io::Result<()> {
    let fds = [fd];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    control.push(SendAncillaryMessage::ScmRights(&fds));
    sendmsg(
        socket,
        &[io::IoSlice::new(&[0])],
        &mut control,
        SendFlags::empty(),
    )?;
    Ok(())
}

/// The file descriptor sent over `socket`, failing when it closes first.
fn recv_fd(socket: &UnixStream) -> io::Result<OwnedFd> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut byte = [0];
    recvmsg(
        socket,
        &mut [io::IoSliceMut::new(&mut byte)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    control
        .drain()
        .find_map(|message| match message {
            RecvAncillaryMessage::ScmRights(mut fds) => fds.next(),
            _ => None,
        })
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
}
