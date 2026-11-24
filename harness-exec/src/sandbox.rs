// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Host-side client for running tool calls inside a sandboxed worker container.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use genai::chat::{ToolCall, ToolResponse};
use hakoniwa::{Child, Stdio};
use harness_sandbox::{Sandbox, SandboxError};
use scopeguard::ScopeGuard;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::protocol::{read_line_bounded, write_line, Request, Response};
use crate::{error_json, lock, ToolDetails};

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
    /// What to write to the worker, taken by the writer thread: a worker
    /// that stopped reading never keeps a caller waiting.
    requests: Option<Sender<Request>>,
    pending: PendingMap,
    alive: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    writer: Option<thread::JoinHandle<()>>,
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
        // Nothing more to write closes stdin, a clean EOF, then SIGKILL
        // guarantees container PID 1 and all descendants terminate
        // immediately without hanging, which also ends a write still waiting.
        self.requests.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for handle in [self.reader.take(), self.writer.take()]
            .into_iter()
            .flatten()
        {
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

        let (pending, mut rx) = match self.send_call(id, req) {
            Ok(pair) => pair,
            Err(err) => {
                return (ToolResponse::from_tool_call(call, error_json(err)), None);
            }
        };

        // If the caller drops this future (e.g. Esc), cancel only this call on
        // the worker that owns it so the worker and its `/tmp` stay alive.
        let cancel_guard = scopeguard::guard((), |()| {
            lock(&pending).remove(&id);
            if let Some(worker) = lock(&self.worker).as_ref() {
                if Arc::ptr_eq(&worker.pending, &pending) {
                    let _ = worker.send(Request::Cancel { id });
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
        let content = error_json("sandboxed tool executor exited unexpectedly");
        (ToolResponse::from_tool_call(call, content), None)
    }

    /// Stop what commands left running in the background, such as a program
    /// started with `setsid`, and wait until it is gone. With `keep_jobs`,
    /// the commands the `bash` tool started in the background keep
    /// running. Call it when no call runs.
    ///
    /// Returns whether such commands were running, as they may have
    /// changed files since, or when this is not known.
    ///
    /// A worker that does not answer in time, as one a command stopped
    /// with `SIGSTOP`, is killed with its container: the next call starts
    /// a new one.
    pub async fn stop_background(&self, keep_jobs: bool) -> bool {
        let alive = lock(&self.worker)
            .as_ref()
            .is_some_and(|w| w.alive.load(Ordering::Acquire));
        // A dead worker took its container down with it.
        if !alive {
            return true;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::StopBackground { id, keep_jobs };
        let Ok((_, mut rx)) = self.send_call(id, req) else {
            return true;
        };
        let stopped = async {
            while let Some(resp) = rx.recv().await {
                if let Response::Result { content, .. } = resp {
                    let jobs = serde_json::from_str::<serde_json::Value>(&content)
                        .ok()
                        .and_then(|v| v["jobs"].as_bool());
                    return jobs.unwrap_or(true);
                }
            }
            true
        };
        match tokio::time::timeout(STOP_TIMEOUT, stopped).await {
            Ok(jobs) => jobs,
            Err(_) => {
                lock(&self.worker).take();
                true
            }
        }
    }

    fn send_call(
        &self,
        id: u64,
        mut req: Request,
    ) -> Result<(PendingMap, mpsc::UnboundedReceiver<Response>), ExecError> {
        let mut guard = lock(&self.worker);

        for attempt in 0..2 {
            let needs_spawn = guard
                .as_ref()
                .is_none_or(|w| !w.alive.load(Ordering::Acquire));
            if needs_spawn {
                // Drop old worker before spawning a replacement.
                guard.take();
                *guard = Some(spawn_worker(&self.config, self.program)?);
            }
            let worker = guard.as_ref().expect("worker just spawned");
            let (tx, rx) = mpsc::unbounded_channel();
            lock(&worker.pending).insert(id, tx);
            match worker.send(req) {
                Ok(()) => return Ok((worker.pending.clone(), rx)),
                Err(unsent) => {
                    lock(&worker.pending).remove(&id);
                    if attempt > 0 {
                        break;
                    }
                    // The worker died: start another one.
                    worker.alive.store(false, Ordering::Release);
                    req = unsent;
                }
            }
        }
        Err(ExecError::Startup("failed to send call to worker".into()))
    }
}

impl Worker {
    /// Queue `req` for the worker, giving it back when a write failed
    /// before, as the worker is then dead.
    fn send(&self, req: Request) -> Result<(), Request> {
        match &self.requests {
            Some(requests) => requests.send(req).map_err(|err| err.0),
            None => Err(req),
        }
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
    let child = start(config)?;
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
        return Err(startup_failure(ScopeGuard::into_inner(child), eof));
    }
    let child = ScopeGuard::into_inner(child);

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let (pending_bg, alive_bg) = (pending.clone(), alive.clone());
    let reader = thread::spawn(move || read_responses(stdout_reader, &pending_bg, &alive_bg));
    let (requests, queued) = channel();
    let alive_bg = alive.clone();
    let writer = thread::spawn(move || write_requests(stdin, &queued, &alive_bg));

    Ok(Worker {
        child,
        requests: Some(requests),
        pending,
        alive,
        reader: Some(reader),
        writer: Some(writer),
    })
}

/// Start the worker program in its container, with pipes to talk to it.
fn start(config: &SandboxConfig) -> Result<Child, ExecError> {
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
    cmd.spawn().map_err(|e| ExecError::Startup(e.to_string()))
}

/// Why the worker `child` did not say it was ready, reaping it. `eof`
/// tells that its output ended.
fn startup_failure(mut child: Child, eof: bool) -> ExecError {
    // When stdout already hit EOF, the worker is exiting on its own: wait
    // without SIGKILL so we preserve its actual exit code / hakoniwa reason.
    if !eof {
        let _ = child.kill();
    }
    ExecError::Startup(match child.wait() {
        Ok(status) if !status.reason.is_empty() => status.reason,
        Ok(status) => format!("worker exited during startup (code {})", status.code),
        Err(err) => err.to_string(),
    })
}

/// Hand each response the worker writes to the call waiting for it, until
/// its output ends: the worker is then dead, and the calls left get no
/// answer.
fn read_responses(mut stdout: impl BufRead, pending: &PendingMap, alive: &AtomicBool) {
    let _done = scopeguard::guard((), |()| {
        alive.store(false, Ordering::Release);
        lock(pending).clear();
    });
    let mut buf = Vec::new();
    while let Ok(1..) = read_line_bounded(&mut stdout, &mut buf) {
        let Ok(resp) = serde_json::from_slice::<Response>(buf.trim_ascii()) else {
            continue;
        };
        let (id, last) = match &resp {
            Response::Output { id, .. } => (*id, false),
            Response::Result { id, .. } => (*id, true),
            Response::Ready => continue,
        };
        let mut map = lock(pending);
        if let Some(tx) = map.get(&id) {
            let _ = tx.send(resp);
        }
        // The result of a call is the last it gets.
        if last {
            map.remove(&id);
        }
    }
}

/// Write each of the `requests` to the worker, until there is none to
/// wait for or a write fails: the worker is then dead.
fn write_requests(mut stdin: impl Write, requests: &Receiver<Request>, alive: &AtomicBool) {
    for req in requests {
        if write_line(&mut stdin, &req).is_err() {
            alive.store(false, Ordering::Release);
            break;
        }
    }
}
