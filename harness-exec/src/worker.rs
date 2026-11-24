// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Async NDJSON tool worker running inside the sandbox container.

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{mpsc, oneshot};

use crate::protocol::{read_line_bounded_async, reset_buf, Request, Response};
use crate::{dispatch_raw, error_json, lock, BashContext, ToolContext};

type InFlightMap = Arc<Mutex<HashMap<u64, (u64, oneshot::Sender<()>)>>>;

/// Run the NDJSON tool worker on `stdin`/`stdout` in `cwd`.
///
/// `background` lets commands be started in the background. `below_init`
/// tells whether it runs below the container's init (see
/// [`harness_sandbox::start_init`]), where it can stop what commands leave
/// running.
///
/// Refuses to run when `stdin` is an interactive terminal so accidentally
/// invoking the internal worker mode fails immediately instead of hanging.
pub async fn run_worker(
    cwd: PathBuf,
    max_memory: Option<u64>,
    background: bool,
    below_init: bool,
) -> io::Result<()> {
    if io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tool executor expects piped stdin",
        ));
    }
    let mut ctx = ToolContext {
        cwd,
        bash: BashContext {
            background,
            ..BashContext::default()
        },
    };
    if let Some(bytes) = max_memory {
        ctx.bash.pre_exec = Some(Arc::new(move || harness_sandbox::limit_memory(bytes)));
    }
    run_worker_io(tokio::io::stdin(), tokio::io::stdout(), ctx, below_init).await
}

/// Core async worker loop decoupled from stdio so it can be tested directly
/// over in-memory duplex pipes. Without `below_init`, stopping background
/// programs does nothing.
pub async fn run_worker_io<R, W>(
    reader: R,
    writer: W,
    ctx: ToolContext,
    below_init: bool,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Response>();
    let writer_task = tokio::spawn(async move {
        let mut out = BufWriter::new(writer);
        let mut buf = Vec::new();
        while let Some(resp) = out_rx.recv().await {
            reset_buf(&mut buf);
            if serde_json::to_writer(&mut buf, &resp).is_err() {
                continue;
            }
            buf.push(b'\n');
            if out.write_all(&buf).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });

    // Signal readiness immediately so the host knows container setup succeeded.
    let _ = out_tx.send(Response::Ready);

    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let mut tasks = Tasks {
        out: out_tx,
        done: done_tx,
        in_flight: Arc::default(),
        ctx,
        below_init,
        seq: 0,
    };
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    while let Ok(1..) = read_line_bounded_async(&mut reader, &mut line).await {
        match serde_json::from_slice::<Request>(line.trim_ascii()) {
            Ok(Request::Call { id, name, args }) => tasks.call(id, name, args),
            Ok(Request::StopBackground { id, keep_jobs }) => tasks.stop_background(id, keep_jobs),
            Ok(Request::Cancel { id }) => tasks.cancel(id),
            // The host writes every request itself: a line that is not
            // one has no call waiting for an answer.
            Err(_) => {}
        }
    }

    // Host closed stdin: cancel all active calls and wait for every task to
    // run its drop guards (reaping bash process groups) before exiting.
    let Tasks {
        out,
        done,
        in_flight,
        ..
    } = tasks;
    for (_, (_, cancel_tx)) in lock(&in_flight).drain() {
        let _ = cancel_tx.send(());
    }
    drop(done);
    let _ = done_rx.recv().await;
    drop(out);
    let _ = writer_task.await;
    Ok(())
}

/// What the tasks answering the requests share.
struct Tasks {
    out: mpsc::UnboundedSender<Response>,
    /// Every task holds a clone and drops it on exit, letting shutdown wait
    /// until all cancelled bash groups are reaped without retaining
    /// JoinHandles across the session.
    done: mpsc::Sender<()>,
    in_flight: InFlightMap,
    ctx: ToolContext,
    below_init: bool,
    /// Tells apart the calls that reuse an id.
    seq: u64,
}

impl Tasks {
    /// Run the tool call `id` in a task, until it ends or is cancelled.
    fn call(&mut self, id: u64, name: String, args: Value) {
        self.seq = self.seq.wrapping_add(1);
        let gen = self.seq;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        if let Some((_, prev)) = lock(&self.in_flight).insert(id, (gen, cancel_tx)) {
            let _ = prev.send(());
        }
        let in_flight = self.in_flight.clone();
        let out = self.out.clone();
        let ctx = self.ctx.clone();
        let done = self.done.clone();
        tokio::spawn(async move {
            let _done = done;
            let _cleanup = scopeguard::guard((), |()| {
                let mut map = lock(&in_flight);
                if map.get(&id).is_some_and(|(g, _)| *g == gen) {
                    map.remove(&id);
                }
            });
            let chunks = out.clone();
            let call = dispatch_raw(&name, &args, &ctx, |chunk| {
                let _ = chunks.send(Response::Output {
                    id,
                    chunk: chunk.to_owned(),
                });
            });
            let (content, details) = tokio::select! {
                result = call => result,
                // Dropping `call` kills any active bash process group.
                _ = cancel_rx => (error_json("cancelled"), None),
            };
            let _ = out.send(Response::Result {
                id,
                content,
                details,
            });
        });
    }

    /// Stop what commands left running in the background, in a task.
    fn stop_background(&self, id: u64, keep_jobs: bool) {
        let out = self.out.clone();
        let done = self.done.clone();
        let jobs = self.ctx.bash.jobs.clone();
        let below_init = self.below_init;
        tokio::spawn(async move {
            let _done = done;
            let had_jobs = jobs.stop_unless(keep_jobs);
            if below_init {
                let keep = jobs.pids();
                let _ =
                    tokio::task::spawn_blocking(move || harness_sandbox::stop_others(&keep)).await;
            }
            let _ = out.send(Response::Result {
                id,
                content: json!({ "jobs": had_jobs }).to_string(),
                details: None,
            });
        });
    }

    /// Cancel the call `id`, if it still runs.
    fn cancel(&self, id: u64) {
        if let Some((_, cancel_tx)) = lock(&self.in_flight).remove(&id) {
            let _ = cancel_tx.send(());
        }
    }
}
