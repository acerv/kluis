// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Host-side client for running tool calls inside a sandboxed worker container.

use std::collections::HashMap;
use std::io::{self, BufReader, PipeWriter};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use genai::chat::{ToolCall, ToolResponse};
use hakoniwa::{Child, Stdio};
use harness_sandbox::{Sandbox, SandboxError};
use scopeguard::ScopeGuard;
use serde_json::json;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::protocol::{read_line_bounded, write_line, Request, Response};
use crate::ToolDetails;

/// Configuration for spawning the sandboxed tool worker container.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub sandbox: Sandbox,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// Error starting the sandboxed tool executor.
#[derive(Debug, Error)]
pub enum ExecError {
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("sandbox: {0}")]
    Io(#[from] io::Error),
    #[error("sandbox: {0}")]
    Startup(String),
}

/// How long the worker may take to stop what runs in the background,
/// before it is taken for stuck.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

type PendingMap = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Response>>>>;

/// A file, as its device, inode and change time: a new file may get the
/// inode of a deleted one, never its change time.
type FileId = (u64, u64, i64, i64);

/// Persistent sandboxed tool executor supporting concurrent async calls.
#[derive(Debug)]
pub struct SandboxExecutor {
    config: SandboxConfig,
    /// The worker program's file when the executor started.
    program: FileId,
    worker: Mutex<Option<Worker>>,
    next_id: AtomicU64,
}

struct Worker {
    child: Child,
    stdin: Option<PipeWriter>,
    pending: PendingMap,
    alive: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("pid", &self.child.id())
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .finish()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing stdin signals clean EOF, then SIGKILL guarantees container
        // PID 1 and all descendants terminate immediately without hanging.
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

impl SandboxExecutor {
    /// Spawn the sandboxed worker eagerly and wait for its readiness signal so
    /// container setup errors (invalid paths, blocked namespaces) fail fast.
    ///
    /// A worker that dies is started again on the next call, only while its
    /// program is still the same file.
    pub fn new(config: SandboxConfig) -> Result<Self, ExecError> {
        let program = file_id(&config.program)?;
        let worker = spawn_worker(&config, program)?;
        Ok(Self {
            config,
            program,
            worker: Mutex::new(Some(worker)),
            next_id: AtomicU64::new(1),
        })
    }

    /// Execute `call` inside the sandboxed worker, streaming live `bash` chunks
    /// via `on_output`. Safe to invoke concurrently from multiple futures.
    pub async fn execute(
        &self,
        call: &ToolCall,
        mut on_output: impl FnMut(&str),
    ) -> (ToolResponse, Option<ToolDetails>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::Call {
            id,
            name: call.fn_name.clone(),
            args: call.fn_arguments.clone(),
        };

        let (pending, mut rx) = match self.send_call(id, &req) {
            Ok(pair) => pair,
            Err(err) => {
                let content = json!({ "error": err.to_string() }).to_string();
                return (ToolResponse::from_tool_call(call, content), None);
            }
        };

        // If the caller drops this future (e.g. Esc), cancel only this call on
        // the worker that owns it so the worker and its `/tmp` stay alive.
        let cancel_guard = scopeguard::guard((), |()| {
            if let Ok(mut map) = pending.lock() {
                map.remove(&id);
            }
            if let Ok(mut guard) = self.worker.lock() {
                if let Some(worker) = guard.as_mut() {
                    if Arc::ptr_eq(&worker.pending, &pending) {
                        if let Some(stdin) = worker.stdin.as_mut() {
                            let _ = write_line(stdin, &Request::Cancel { id });
                        }
                    }
                }
            }
        });

        while let Some(resp) = rx.recv().await {
            match resp {
                Response::Output { chunk, .. } => on_output(&chunk),
                Response::Result {
                    content, details, ..
                } => {
                    ScopeGuard::into_inner(cancel_guard);
                    return (ToolResponse::from_tool_call(call, content), details);
                }
                Response::Ready => {}
            }
        }

        ScopeGuard::into_inner(cancel_guard);
        let content = json!({ "error": "sandboxed tool executor exited unexpectedly" }).to_string();
        (ToolResponse::from_tool_call(call, content), None)
    }

    /// Stop what commands left running in the background, such as a program
    /// started with `setsid`, and wait until it is gone. Call it when no
    /// call runs.
    ///
    /// A worker that does not answer in time, as one a command stopped
    /// with `SIGSTOP`, is killed with its container: the next call starts
    /// a new one.
    pub async fn stop_background(&self) {
        let alive = self.worker.lock().is_ok_and(|guard| {
            guard
                .as_ref()
                .is_some_and(|w| w.alive.load(Ordering::Acquire))
        });
        // A dead worker took its container down with it.
        if !alive {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let Ok((_, mut rx)) = self.send_call(id, &Request::StopBackground { id }) else {
            return;
        };
        let stopped = async {
            while let Some(resp) = rx.recv().await {
                if matches!(resp, Response::Result { .. }) {
                    break;
                }
            }
        };
        if tokio::time::timeout(STOP_TIMEOUT, stopped).await.is_err() {
            if let Ok(mut guard) = self.worker.lock() {
                guard.take();
            }
        }
    }

    fn send_call(
        &self,
        id: u64,
        req: &Request,
    ) -> Result<(PendingMap, mpsc::UnboundedReceiver<Response>), ExecError> {
        let mut guard = self
            .worker
            .lock()
            .map_err(|_| ExecError::Startup("executor lock poisoned".into()))?;

        for attempt in 0..2 {
            let needs_spawn = guard
                .as_ref()
                .is_none_or(|w| !w.alive.load(Ordering::Acquire));
            if needs_spawn {
                // Drop old worker before spawning a replacement.
                guard.take();
                *guard = Some(spawn_worker(&self.config, self.program)?);
            }
            let worker = guard.as_mut().expect("worker just spawned");
            let (tx, rx) = mpsc::unbounded_channel();
            if let Ok(mut map) = worker.pending.lock() {
                map.insert(id, tx);
            }
            let write_res = worker
                .stdin
                .as_mut()
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker stdin closed"))
                .and_then(|stdin| write_line(stdin, req));
            match write_res {
                Ok(()) => return Ok((worker.pending.clone(), rx)),
                Err(err) if attempt == 0 => {
                    if let Ok(mut map) = worker.pending.lock() {
                        map.remove(&id);
                    }
                    worker.alive.store(false, Ordering::Release);
                    let _ = err;
                }
                Err(err) => {
                    if let Ok(mut map) = worker.pending.lock() {
                        map.remove(&id);
                    }
                    return Err(ExecError::Io(err));
                }
            }
        }
        Err(ExecError::Startup("failed to send call to worker".into()))
    }
}

/// The file at `path`.
fn file_id(path: &Path) -> io::Result<FileId> {
    let meta = std::fs::metadata(path)?;
    Ok((meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()))
}

/// Start the worker, if its program is still the file `expected`.
fn spawn_worker(config: &SandboxConfig, expected: FileId) -> Result<Worker, ExecError> {
    // In a writable folder, a command could have renamed another program
    // over it, which would run without the worker's own restrictions. No
    // command can swap it before the start: they all died with the last
    // worker's container.
    if file_id(&config.program)? != expected {
        return Err(ExecError::Startup(format!(
            "{}: replaced since the sandbox started; start again to use it",
            config.program.display()
        )));
    }
    let program = config
        .program
        .to_str()
        .ok_or_else(|| ExecError::Startup("worker path is not UTF-8".into()))?;
    let container = config.sandbox.container()?;
    let dev_null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    let mut cmd = container.command(program);
    cmd.args(&config.args)
        .envs(config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(dev_null));

    let child = cmd.spawn().map_err(|e| ExecError::Startup(e.to_string()))?;
    // Ensure `child` is killed and reaped on any early error before `Worker`
    // takes ownership.
    let mut child = scopeguard::guard(child, |mut c| {
        let _ = c.kill();
        let _ = c.wait();
    });
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ExecError::Startup("missing worker stdin pipe".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExecError::Startup("missing worker stdout pipe".into()))?;

    let mut stdout_reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let read_res = read_line_bounded(&mut stdout_reader, &mut line);
    let eof = matches!(read_res, Ok(0));
    let ready = read_res
        .ok()
        .filter(|&n| n > 0)
        .and_then(|_| serde_json::from_slice::<Response>(line.trim_ascii()).ok());
    if !matches!(ready, Some(Response::Ready)) {
        drop(stdin);
        drop(stdout_reader);
        let mut child = ScopeGuard::into_inner(child);
        // When stdout already hit EOF, the worker is exiting on its own: wait
        // without SIGKILL so we preserve its actual exit code / hakoniwa reason.
        if !eof {
            let _ = child.kill();
        }
        let detail = match child.wait() {
            Ok(status) if !status.reason.is_empty() => status.reason,
            Ok(status) => format!("worker exited during startup (code {})", status.code),
            Err(err) => err.to_string(),
        };
        return Err(ExecError::Startup(detail));
    }
    let child = ScopeGuard::into_inner(child);

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let pending_bg = pending.clone();
    let alive_bg = alive.clone();

    let reader = thread::spawn(move || {
        let _done = scopeguard::guard((), |()| {
            alive_bg.store(false, Ordering::Release);
            if let Ok(mut map) = pending_bg.lock() {
                map.clear();
            }
        });
        let mut buf = Vec::new();
        while let Ok(n) = read_line_bounded(&mut stdout_reader, &mut buf) {
            if n == 0 {
                break;
            }
            let trimmed = buf.trim_ascii();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(resp) = serde_json::from_slice::<Response>(trimmed) else {
                continue;
            };
            match &resp {
                Response::Output { id, .. } => {
                    if let Ok(map) = pending_bg.lock() {
                        if let Some(tx) = map.get(id) {
                            let _ = tx.send(resp);
                        }
                    }
                }
                Response::Result { id, .. } => {
                    if let Ok(mut map) = pending_bg.lock() {
                        if let Some(tx) = map.remove(id) {
                            let _ = tx.send(resp);
                        }
                    }
                }
                Response::Ready => {}
            }
        }
    });

    Ok(Worker {
        child,
        stdin: Some(stdin),
        pending,
        alive,
        reader: Some(reader),
    })
}
