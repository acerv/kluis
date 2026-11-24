// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Seek, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anstyle_parse::{Parser, Perform};
use genai::chat::Tool;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::{setsid, Pid};
use scopeguard::ScopeGuard;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tempfile::{Builder, NamedTempFile, TempPath};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;
use tokio::process::{Child, Command};
use tokio::time::{sleep_until, Instant};

use crate::error::ToolError;
use crate::lock;

/// Name of the `bash` tool.
pub const BASH_TOOL: &str = "bash";

/// Bytes kept from the end of the output.
pub const MAX_BASH_OUTPUT: usize = 16 * 1024;

/// Lines kept from the end of the output.
pub const MAX_BASH_LINES: usize = 500;

/// Maximum bytes saved to the spill file when output is truncated.
pub const MAX_BASH_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Time limit in seconds when the call sets none.
pub const DEFAULT_BASH_TIMEOUT: u64 = 600;

/// How long to keep reading after the shell exits while background jobs
/// still write; reset by every chunk of output.
const EXIT_IDLE: Duration = Duration::from_millis(100);

/// Longest time to keep reading after the shell exits, so a background
/// job that never stops writing cannot hold the tool.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Code run in the command's process just before `exec`.
pub type PreExec = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;

/// Files holding the whole output of commands whose output was cut, removed
/// once the last clone is dropped.
#[derive(Clone, Debug, Default)]
pub struct OutputFiles(Arc<Mutex<Vec<TempPath>>>);

/// Commands started in the background, by the pid of their process
/// group; the groups still running are killed once the last clone is
/// dropped.
#[derive(Clone, Debug, Default)]
pub struct Jobs(Arc<JobList>);

#[derive(Debug, Default)]
struct JobList {
    pids: Mutex<Vec<u32>>,
}

impl Drop for JobList {
    fn drop(&mut self) {
        if let Ok(pids) = self.pids.get_mut() {
            pids.drain(..).for_each(|pid| kill_group(Some(pid)));
        }
    }
}

impl Jobs {
    /// The process groups of the commands still running, which are also
    /// their sessions.
    pub fn pids(&self) -> Vec<u32> {
        lock(&self.0.pids).clone()
    }

    /// Kill every command still running.
    pub fn stop(&self) {
        lock(&self.0.pids)
            .drain(..)
            .for_each(|pid| kill_group(Some(pid)));
    }

    /// Kill every command still running unless `keep`, and return whether
    /// any was.
    pub fn stop_unless(&self, keep: bool) -> bool {
        let running = !self.pids().is_empty();
        if !keep {
            self.stop();
        }
        running
    }
}

/// How the host wants commands to run.
#[derive(Clone, Default)]
pub struct BashContext {
    /// Runs in the command's process before `exec`, after it gets its own
    /// session, e.g. to apply a sandbox. It runs between `fork` and `exec`,
    /// so it must not allocate or take locks. An error stops the command.
    pub pre_exec: Option<PreExec>,
    /// The only environment variables of the command when set; it inherits
    /// the host environment otherwise.
    pub env: Option<Vec<(OsString, OsString)>>,
    /// Removes the files of cut outputs once dropped, when set; they stay
    /// for good otherwise.
    pub output_files: Option<OutputFiles>,
    /// Whether commands may be started in the background.
    pub background: bool,
    /// Commands started in the background.
    pub jobs: Jobs,
}

impl BashContext {
    /// Keep the output file at `path` as long as the host wants, and
    /// return its name.
    fn keep(&self, path: TempPath) -> Option<String> {
        let name = path.to_str()?.to_owned();
        match &self.output_files {
            Some(files) => lock(&files.0).push(path),
            None => {
                path.keep().ok()?;
            }
        }
        Some(name)
    }
}

impl fmt::Debug for BashContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only the names: values may be secret.
        let env = self
            .env
            .as_ref()
            .map(|env| env.iter().map(|(k, _)| k).collect::<Vec<_>>());
        f.debug_struct("BashContext")
            .field("pre_exec", &self.pre_exec.is_some())
            .field("env", &env)
            .field("output_files", &self.output_files.is_some())
            .field("background", &self.background)
            .field("jobs", &self.jobs.pids())
            .finish()
    }
}

/// A call of the `bash` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashTool {
    pub command: String,
    /// Time limit in seconds; [`DEFAULT_BASH_TIMEOUT`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Keep the command running after the call, without time limit; see
    /// [`start_in`](Self::start_in).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashOutput {
    /// Exit code, or 128 + signal number when killed by a signal. `None`
    /// when the shell was still running at the timeout.
    pub exit_code: Option<i32>,
    /// Stdout and stderr in the order they were written, without colors
    /// and control characters.
    pub output: String,
    /// True when only the end of the output is kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Path to a file with the full cleaned output (up to [`MAX_BASH_LOG_BYTES`])
    /// when truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
}

/// A command started in the background.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobOutput {
    /// Its process group, which also is its session.
    pub pid: u32,
    /// File with its cleaned output, restarted once it reaches
    /// [`MAX_BASH_LOG_BYTES`].
    pub output_file: String,
}

impl BashTool {
    /// JSON Schema of the tool input, with `background` when it may be
    /// set.
    pub fn schema(background: bool) -> Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "minimum": 1 }
            },
            "required": ["command"]
        });
        if background {
            schema["properties"]["background"] = json!({ "type": "boolean" });
        }
        schema
    }

    /// The `bash` tool, as sent to the model, offering `background` when
    /// it may be set.
    pub fn definition(background: bool) -> Tool {
        let mut description = format!(
            "Run a bash command and return its exit code and output. \
             Optionally set timeout in seconds (default {DEFAULT_BASH_TIMEOUT})."
        );
        if background {
            description.push_str(
                " Set background to keep it running, as a server: you get its \
                 pid and a file with its output; stop it with `kill -- -PID`.",
            );
        }
        Tool::new(BASH_TOOL)
            .with_description(description)
            .with_schema(Self::schema(background))
    }

    /// Run the command with `bash -c`.
    ///
    /// Stdin is closed and stderr shares the pipe of stdout. The command
    /// runs in a new session without a controlling terminal, killed
    /// as a whole on timeout, on error, or when this future is dropped.
    /// Background jobs keep running after the shell exits; their output is
    /// read until it stays quiet for a moment. The command starts in the
    /// current directory.
    pub async fn run(self) -> Result<BashOutput, ToolError> {
        self.run_with(&BashContext::default()).await
    }

    /// Like [`run`](Self::run), with the host's [`BashContext`].
    pub async fn run_with(self, ctx: &BashContext) -> Result<BashOutput, ToolError> {
        self.run_in(Path::new("."), ctx).await
    }

    /// Like [`run_with`](Self::run_with), starting the command in `cwd`.
    pub async fn run_in(self, cwd: &Path, ctx: &BashContext) -> Result<BashOutput, ToolError> {
        self.run_in_streaming(cwd, ctx, |_| {}).await
    }

    /// Like [`run_in`](Self::run_in), calling `on_output` with each cleaned
    /// UTF-8 chunk of stdout/stderr as it arrives.
    pub async fn run_in_streaming(
        self,
        cwd: &Path,
        ctx: &BashContext,
        mut on_output: impl FnMut(&str),
    ) -> Result<BashOutput, ToolError> {
        if self.timeout == Some(0) {
            return Err(ToolError::new("timeout must be >= 1"));
        }
        check_dir(cwd).await?;
        // A timeout too large to represent means no limit.
        let deadline = Instant::now().checked_add(Duration::from_secs(
            self.timeout.unwrap_or(DEFAULT_BASH_TIMEOUT),
        ));

        let (mut child, pipe) = spawn(&self.command, cwd, ctx).map_err(bash_error)?;
        let group = scopeguard::guard(child.id(), kill_group);
        let mut pipe = Some(pipe);
        let mut filter = Filter::default();
        let mut tail = Tail::default();
        let mut status: Option<ExitStatus> = None;
        let mut chunk = [0u8; 8192];
        let mut idle = Instant::now();
        let mut exited_at = idle;

        let timed_out = loop {
            if status.is_some() && pipe.is_none() {
                break false;
            }
            tokio::select! {
                read = read_some(&mut pipe, &mut chunk), if pipe.is_some() => {
                    match read.map_err(bash_error)? {
                        0 => pipe = None,
                        n => {
                            filter.clean_into(&chunk[..n], &mut tail, &mut on_output);
                            if status.is_some() {
                                idle = (Instant::now() + EXIT_IDLE).min(exited_at + EXIT_GRACE);
                            }
                        }
                    }
                }
                exited = child.wait(), if status.is_none() => {
                    status = Some(exited.map_err(bash_error)?);
                    exited_at = Instant::now();
                    idle = exited_at + EXIT_IDLE;
                }
                // Background jobs still hold the pipe but stopped writing.
                _ = sleep_until(idle), if status.is_some() => break false,
                _ = sleep_until(deadline.unwrap_or(idle)), if deadline.is_some() => break true,
            }
        };

        if timed_out {
            drop(group);
            if status.is_none() {
                // The group kill already hit the shell; reap it.
                let _ = child.kill().await;
            }
        } else {
            // Finished normally: background jobs may keep running.
            ScopeGuard::into_inner(group);
        }

        let (output, truncated, output_file) = tail.finish();
        Ok(BashOutput {
            exit_code: status.map(exit_code),
            output,
            truncated,
            output_file: output_file.and_then(|path| ctx.keep(path)),
            timed_out,
        })
    }

    /// Start the command in `cwd` and return at once, leaving it running
    /// in its own session, without time limit, until it ends, the host
    /// kills it, or the last clone of `ctx.jobs` is dropped. Its cleaned
    /// output goes to a file. `timeout` does not apply. Fails unless
    /// `ctx.background` allows it.
    pub async fn start_in(self, cwd: &Path, ctx: &BashContext) -> Result<JobOutput, ToolError> {
        if !ctx.background {
            return Err(ToolError::new("background commands are not enabled"));
        }
        check_dir(cwd).await?;
        let file = Builder::new()
            .prefix("harness-job-")
            .suffix(".log")
            .tempfile()
            .map_err(bash_error)?;
        let (child, pipe) = spawn(&self.command, cwd, ctx).map_err(bash_error)?;
        let pid = child
            .id()
            .ok_or_else(|| ToolError::new("bash: exited at once"))?;
        let (file, path) = file.into_parts();
        let Some(output_file) = ctx.keep(path) else {
            kill_group(Some(pid));
            return Err(ToolError::new("bash: cannot keep the output file"));
        };
        lock(&ctx.jobs.0.pids).push(pid);
        tokio::spawn(log_job(child, pipe, file, Arc::downgrade(&ctx.jobs.0)));
        Ok(JobOutput { pid, output_file })
    }
}

/// Write the cleaned output of a background command to `file` until it
/// ends, then take it off `jobs`.
async fn log_job(mut child: Child, mut pipe: Receiver, mut file: File, jobs: Weak<JobList>) {
    let pid = child.id();
    let log = async {
        let mut filter = Filter::default();
        let mut text = Tail::default();
        let mut chunk = [0u8; 8192];
        let mut written = 0;
        let mut failed = false;
        while let Ok(n @ 1..) = pipe.read(&mut chunk).await {
            filter.clean(&chunk[..n], &mut text);
            if !failed {
                failed = append(&mut file, &mut written, &text.buf).is_err();
            }
            text.buf.clear();
        }
    };
    let _ = tokio::join!(log, child.wait());
    if let Some(jobs) = jobs.upgrade() {
        lock(&jobs.pids).retain(|&p| Some(p) != pid);
    }
}

/// Append `data` to `file`, which holds `written` bytes, starting it again
/// when it would grow past [`MAX_BASH_LOG_BYTES`].
fn append(file: &mut File, written: &mut u64, data: &[u8]) -> io::Result<()> {
    const RESTARTED: &[u8] = b"[earlier output dropped]\n";
    if *written + data.len() as u64 > MAX_BASH_LOG_BYTES {
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(RESTARTED)?;
        *written = RESTARTED.len() as u64;
    }
    // A single chunk is far below the limit.
    file.write_all(data)?;
    *written += data.len() as u64;
    Ok(())
}

/// Fail like a missing bash would when `cwd` is not a folder.
async fn check_dir(cwd: &Path) -> Result<(), ToolError> {
    match tokio::fs::metadata(cwd).await {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => {
            let cwd = cwd.display();
            Err(ToolError::new(format!("{cwd}: not a directory")))
        }
        Err(e) => Err(ToolError::io(&cwd.display().to_string(), e)),
    }
}

/// Start `command` in `cwd` with stdout and stderr on a single pipe, as
/// `ctx` asks.
fn spawn(command: &str, cwd: &Path, ctx: &BashContext) -> io::Result<(Child, Receiver)> {
    let (reader, writer) = io::pipe()?;
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .kill_on_drop(true);
    if let Some(env) = &ctx.env {
        cmd.env_clear().envs(env.iter().cloned());
    }
    let hook = ctx.pre_exec.clone();
    // SAFETY: setsid is async-signal-safe, and the hook must be too (see
    // `BashContext`). A new session has no controlling terminal, so
    // commands reading /dev/tty fail instead of stopping, and its process
    // group can be killed as a whole.
    unsafe {
        cmd.pre_exec(move || {
            setsid().map_err(io::Error::from)?;
            match &hook {
                Some(hook) => hook(),
                None => Ok(()),
            }
        });
    }
    let child = cmd.spawn()?;
    // The command still holds the write ends; close them so the read end
    // sees the end of the output.
    drop(cmd);
    let reader = Receiver::from_owned_fd(reader.into())?;
    Ok((child, reader))
}

fn bash_error(e: io::Error) -> ToolError {
    ToolError::new(format!("bash: {e}"))
}

async fn read_some(pipe: &mut Option<Receiver>, chunk: &mut [u8]) -> io::Result<usize> {
    match pipe {
        Some(pipe) => pipe.read(chunk).await,
        None => Ok(0),
    }
}

/// Exit code, using the shell convention 128 + signal for killed processes.
fn exit_code(status: ExitStatus) -> i32 {
    match status.signal() {
        Some(signal) => 128 + signal,
        None => status.code().unwrap_or(-1),
    }
}

/// Kills the command's process group if a pid was recorded.
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

/// Removes ANSI escape sequences (colors, cursor moves, titles) and
/// control characters other than tab, newline and carriage return.
///
/// Keeps parser state between chunks, so sequences split across reads are
/// removed too.
#[derive(Default)]
struct Filter {
    parser: Parser,
}

impl Filter {
    /// Append the cleaned `input` to `tail`, without trimming it.
    fn clean(&mut self, input: &[u8], tail: &mut Tail) {
        for &byte in input {
            self.parser.advance(tail, byte);
        }
    }

    fn clean_into(&mut self, input: &[u8], tail: &mut Tail, on_output: &mut impl FnMut(&str)) {
        let start = tail.buf.len();
        self.clean(input, tail);
        if tail.buf.len() > start {
            if let Ok(text) = std::str::from_utf8(&tail.buf[start..]) {
                on_output(text);
            }
        }
        tail.trim_batch();
    }
}

impl Perform for Tail {
    fn print(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.buf
            .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\t' | b'\n' | b'\r') {
            self.buf.push(byte);
        }
    }
}

/// The end of the output: the last [`MAX_BASH_OUTPUT`] bytes, then the
/// last [`MAX_BASH_LINES`] lines. Overflowing bytes are streamed to a
/// temporary file so callers can inspect the full output without re-running.
#[derive(Default)]
struct Tail {
    buf: Vec<u8>,
    cut: bool,
    spill: Spill,
}

impl Tail {
    /// Trim in batches, so long outputs are not moved on every read.
    fn trim_batch(&mut self) {
        if self.buf.len() > 2 * MAX_BASH_OUTPUT {
            self.trim_bytes();
        }
    }

    fn trim_bytes(&mut self) {
        if self.buf.len() > MAX_BASH_OUTPUT {
            let cut = self.buf.len() - MAX_BASH_OUTPUT;
            self.spill.write(&self.buf[..cut]);
            self.buf.drain(..cut);
            self.cut = true;
        }
    }

    fn trim_lines(&mut self) {
        let last = self.buf.len().saturating_sub(1);
        if let Some(i) = memchr::memrchr_iter(b'\n', &self.buf[..last]).nth(MAX_BASH_LINES - 1) {
            self.spill.write(&self.buf[..=i]);
            self.buf.drain(..=i);
            self.cut = true;
        }
    }

    /// The kept text, whether anything was cut, and the spill file if one
    /// was saved, removed when dropped.
    fn finish(mut self) -> (String, bool, Option<TempPath>) {
        self.trim_bytes();
        self.trim_lines();
        // Flush the kept tail before skipping leading UTF-8 continuation bytes,
        // so the spill file preserves characters split across a byte cut.
        let output_file = self.spill.finish(&self.buf);
        if self.cut {
            // Do not start in the middle of a UTF-8 character.
            let skip = self.buf.iter().take_while(|&&b| b & 0xc0 == 0x80).count();
            self.buf.drain(..skip);
        }
        let text = match String::from_utf8(self.buf) {
            Ok(text) => text,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        (text, self.cut, output_file)
    }
}

/// Lazily created file holding the full cleaned output when truncated.
///
/// Its temporary file is removed when dropped (e.g. on cancellation or
/// error), as the one [`finish`](Self::finish) returns.
#[derive(Default)]
enum Spill {
    #[default]
    Idle,
    Writing {
        file: NamedTempFile,
        remaining: u64,
    },
    /// Reached [`MAX_BASH_LOG_BYTES`]; the file is closed and kept on disk.
    Full(TempPath),
    Failed,
}

impl Spill {
    fn write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if matches!(self, Self::Idle) {
            match Builder::new()
                .prefix("harness-bash-")
                .suffix(".log")
                .tempfile()
            {
                Ok(file) => {
                    *self = Self::Writing {
                        file,
                        remaining: MAX_BASH_LOG_BYTES,
                    };
                }
                Err(_) => {
                    *self = Self::Failed;
                    return;
                }
            }
        }
        let Self::Writing { file, remaining } = self else {
            return;
        };
        let slice = &data[..data.len().min(*remaining as usize)];
        if file.write_all(slice).is_err() {
            // Transitioning to Failed drops the `NamedTempFile`, freeing `/tmp`
            // space (e.g. on ENOSPC) instead of leaving a broken partial file.
            *self = Self::Failed;
            return;
        }
        *remaining -= slice.len() as u64;
        if *remaining == 0 {
            if let Self::Writing { file, .. } = std::mem::replace(self, Self::Failed) {
                *self = Self::Full(file.into_temp_path());
            }
        }
    }

    fn finish(mut self, tail: &[u8]) -> Option<TempPath> {
        if matches!(self, Self::Writing { .. }) {
            self.write(tail);
        }
        match self {
            // Close the descriptor before exposing the path.
            Self::Writing { file, .. } => Some(file.into_temp_path()),
            Self::Full(path) => Some(path),
            Self::Idle | Self::Failed => None,
        }
    }
}
