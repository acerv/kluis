// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Leaving in order when asked to stop by a signal.

use std::io;

use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::watch;

use crate::terminal::restore_terminal;

/// Tells when `SIGHUP`, `SIGINT` or `SIGTERM` asks to stop, so that the
/// caller leaves the way it does when the user quits: what a prompt started
/// is stopped and checked first. A second signal exits at once, with
/// `128 + signal`.
///
/// In raw mode `Ctrl-C` is a key, not a signal, so the terminal UI only
/// gets `SIGINT` from outside.
#[derive(Clone)]
pub struct Signals(watch::Receiver<Option<i32>>);

impl Signals {
    /// Start listening, until every clone is dropped. Must be called inside
    /// a Tokio runtime.
    pub fn listen() -> io::Result<Self> {
        let mut sources = Sources {
            hangup: signal(SignalKind::hangup())?,
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        };
        let (tx, rx) = watch::channel(None);
        tokio::spawn(async move {
            loop {
                let signal = tokio::select! {
                    () = tx.closed() => return,
                    signal = sources.next() => signal,
                };
                if tx.borrow().is_some() {
                    restore_terminal();
                    std::process::exit(128 + signal);
                }
                let _ = tx.send(Some(signal));
            }
        });
        Ok(Self(rx))
    }

    /// Wait for the signal asking to stop; it is returned again by every
    /// later call.
    pub async fn recv(&mut self) -> i32 {
        match self.0.wait_for(Option::is_some).await.map(|signal| *signal) {
            Ok(Some(signal)) => signal,
            // The listener only ends with the runtime.
            _ => std::future::pending().await,
        }
    }

    /// The signal that asked to stop, if any did.
    pub fn received(&self) -> Option<i32> {
        *self.0.borrow()
    }
}

struct Sources {
    hangup: Signal,
    interrupt: Signal,
    terminate: Signal,
}

impl Sources {
    async fn next(&mut self) -> i32 {
        let kind = tokio::select! {
            _ = self.hangup.recv() => SignalKind::hangup(),
            _ = self.interrupt.recv() => SignalKind::interrupt(),
            _ = self.terminate.recv() => SignalKind::terminate(),
        };
        kind.as_raw_value()
    }
}
