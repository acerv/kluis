// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Async NDJSON tool worker running inside the sandbox container.

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{mpsc, oneshot};

use crate::protocol::{read_line_bounded_async, reset_buf, Request, Response};
use crate::{dispatch_raw, BashContext, ToolContext};

type InFlightMap = Arc<Mutex<HashMap<u64, (u64, oneshot::Sender<()>)>>>;

/// Run the NDJSON tool worker on `stdin`/`stdout` in `cwd`.
///
/// `below_init` tells whether it runs below the container's init (see
/// [`harness_sandbox::start_init`]), where it can stop what commands leave
/// running.
///
/// Refuses to run when `stdin` is an interactive terminal so accidentally
/// invoking the internal worker mode fails immediately instead of hanging.
pub async fn run_worker(cwd: PathBuf, max_memory: Option<u64>, below_init: bool) -> io::Result<()> {
    if io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tool executor expects piped stdin",
        ));
    }
    let mut ctx = ToolContext {
        cwd,
        bash: BashContext::default(),
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

    // Every spawned task holds a clone of `tasks_done_tx` and drops it on exit,
    // letting shutdown wait until all cancelled bash groups are reaped without
    // retaining JoinHandles across the session.
    let (tasks_done_tx, mut tasks_done_rx) = mpsc::channel::<()>(1);
    let in_flight: InFlightMap = Arc::new(Mutex::new(HashMap::new()));
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut seq = 0u64;

    loop {
        match read_line_bounded_async(&mut reader, &mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_ascii();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_slice::<Request>(trimmed) {
                    Ok(Request::Call { id, name, args }) => {
                        seq = seq.wrapping_add(1);
                        let gen = seq;
                        let (cancel_tx, cancel_rx) = oneshot::channel();
                        if let Ok(mut map) = in_flight.lock() {
                            if let Some((_, prev)) = map.insert(id, (gen, cancel_tx)) {
                                let _ = prev.send(());
                            }
                        }
                        let in_flight_ref = in_flight.clone();
                        let out_tx_task = out_tx.clone();
                        let ctx_task = ctx.clone();
                        let done_token = tasks_done_tx.clone();
                        tokio::spawn(async move {
                            let _done = done_token;
                            let _cleanup = scopeguard::guard((), |()| {
                                if let Ok(mut map) = in_flight_ref.lock() {
                                    if map.get(&id).is_some_and(|(g, _)| *g == gen) {
                                        map.remove(&id);
                                    }
                                }
                            });
                            let chunk_tx = out_tx_task.clone();
                            let call_fut = dispatch_raw(&name, &args, &ctx_task, |chunk| {
                                let _ = chunk_tx.send(Response::Output {
                                    id,
                                    chunk: chunk.to_owned(),
                                });
                            });
                            tokio::select! {
                                (content, details) = call_fut => {
                                    let _ = out_tx_task.send(Response::Result {
                                        id,
                                        content,
                                        details,
                                    });
                                }
                                _ = cancel_rx => {
                                    // Dropping `call_fut` kills any active bash process group.
                                    let _ = out_tx_task.send(Response::Result {
                                        id,
                                        content: json!({ "error": "cancelled" }).to_string(),
                                        details: None,
                                    });
                                }
                            }
                        });
                    }
                    Ok(Request::StopBackground { id, keep_jobs }) => {
                        let out_tx_task = out_tx.clone();
                        let done_token = tasks_done_tx.clone();
                        let jobs = ctx.bash.jobs.clone();
                        tokio::spawn(async move {
                            let _done = done_token;
                            let had_jobs = jobs.stop_unless(keep_jobs);
                            if below_init {
                                let keep = jobs.pids();
                                let _ = tokio::task::spawn_blocking(move || {
                                    harness_sandbox::stop_others(&keep)
                                })
                                .await;
                            }
                            let _ = out_tx_task.send(Response::Result {
                                id,
                                content: json!({ "jobs": had_jobs }).to_string(),
                                details: None,
                            });
                        });
                    }
                    Ok(Request::Cancel { id }) => {
                        if let Ok(mut map) = in_flight.lock() {
                            if let Some((_, cancel_tx)) = map.remove(&id) {
                                let _ = cancel_tx.send(());
                            }
                        }
                    }
                    Err(err) => {
                        let _ = out_tx.send(Response::Result {
                            id: 0,
                            content: json!({ "error": format!("invalid request: {err}") })
                                .to_string(),
                            details: None,
                        });
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Host closed stdin: cancel all active calls and wait for every task to
    // run its drop guards (reaping bash process groups) before exiting.
    if let Ok(mut map) = in_flight.lock() {
        for (_, (_, cancel_tx)) in map.drain() {
            let _ = cancel_tx.send(());
        }
    }
    drop(tasks_done_tx);
    let _ = tasks_done_rx.recv().await;
    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}
