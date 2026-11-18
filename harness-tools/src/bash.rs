// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;
use tokio::process::{Child, Command};
use tokio::time::{sleep_until, Instant};

use crate::error::ToolError;

/// Bytes kept from the end of the output.
pub const MAX_BASH_OUTPUT: usize = 50 * 1024;

/// Lines kept from the end of the output.
pub const MAX_BASH_LINES: usize = 2000;

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
    pub truncated: bool,
    pub timed_out: bool,
}

impl BashTool {
    /// JSON Schema of the tool input.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "minimum": 1 }
            },
            "required": ["command"]
        })
    }

    /// The `bash` tool, as sent to the model.
    pub fn definition() -> Tool {
        Tool::new("bash")
            .with_description(format!(
                "Run a bash command and return its exit code and output. \
                 Optionally set timeout in seconds (default {DEFAULT_BASH_TIMEOUT})."
            ))
            .with_schema(Self::schema())
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
        if self.timeout == Some(0) {
            return Err(ToolError::new("timeout must be >= 1"));
        }
        // Spawning in a missing folder fails like a missing bash would.
        match tokio::fs::metadata(cwd).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                let cwd = cwd.display();
                return Err(ToolError::new(format!("{cwd}: not a directory")));
            }
            Err(e) => return Err(ToolError::io(&cwd.display().to_string(), e)),
        }
        // A timeout too large to represent means no limit.
        let deadline = Instant::now().checked_add(Duration::from_secs(
            self.timeout.unwrap_or(DEFAULT_BASH_TIMEOUT),
        ));

        let (mut child, pipe) = spawn(&self.command, cwd, ctx).map_err(bash_error)?;
        let mut group = GroupGuard(child.id());
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
                            filter.clean_into(&chunk[..n], &mut tail);
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
            group.kill();
            if status.is_none() {
                // The group kill already hit the shell; reap it.
                let _ = child.kill().await;
            }
        } else {
            // Finished normally: background jobs may keep running.
            group.disarm();
        }

        let (output, truncated) = tail.finish();
        Ok(BashOutput {
            exit_code: status.map(exit_code),
            output,
            truncated,
            timed_out,
        })
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
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
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

/// Kills the command's process group when dropped, unless disarmed.
struct GroupGuard(Option<u32>);

impl GroupGuard {
    fn kill(&mut self) {
        if let Some(pid) = self.0.take().and_then(|p| libc::pid_t::try_from(p).ok()) {
            // SAFETY: killpg only sends a signal; an empty group yields ESRCH.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Removes ANSI escape sequences (colors, cursor moves, titles) and
/// control characters other than tab, newline and carriage return.
///
/// Works on raw bytes and keeps its state between chunks, so sequences
/// split across reads are removed too.
#[derive(Default)]
struct Filter {
    state: State,
}

#[derive(Default, Clone, Copy)]
enum State {
    #[default]
    Text,
    /// After ESC.
    Escape,
    /// Inside `ESC [ ...`, ended by a byte in 0x40..=0x7e.
    Csi,
    /// Inside `ESC ] ...` or another string sequence, ended by BEL or
    /// `ESC \`.
    Str,
    /// After ESC inside a string sequence.
    StrEscape,
}

impl Filter {
    fn clean_into(&mut self, input: &[u8], tail: &mut Tail) {
        for &b in input {
            self.state = match (self.state, b) {
                (State::Text, 0x1b) => State::Escape,
                (State::Text, b'\t' | b'\n' | b'\r') => {
                    tail.buf.push(b);
                    State::Text
                }
                (State::Text, 0x00..=0x1f | 0x7f) => State::Text,
                (State::Text, _) => {
                    tail.buf.push(b);
                    State::Text
                }
                (State::Escape, b'[') => State::Csi,
                (State::Escape, b']' | b'P' | b'X' | b'^' | b'_') => State::Str,
                // Two-byte sequences such as `ESC c`.
                (State::Escape, _) => State::Text,
                (State::Csi, 0x20..=0x3f) => State::Csi,
                (State::Csi, _) => State::Text,
                (State::Str, 0x07) => State::Text,
                (State::Str, 0x1b) => State::StrEscape,
                (State::Str, _) => State::Str,
                (State::StrEscape, b'\\') => State::Text,
                (State::StrEscape, _) => State::Str,
            };
        }
        tail.trim_batch();
    }
}

/// The end of the output: the last [`MAX_BASH_OUTPUT`] bytes, then the
/// last [`MAX_BASH_LINES`] lines.
#[derive(Default)]
struct Tail {
    buf: Vec<u8>,
    cut: bool,
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
            self.buf.drain(..self.buf.len() - MAX_BASH_OUTPUT);
            self.cut = true;
        }
    }

    fn trim_lines(&mut self) {
        let last = self.buf.len().saturating_sub(1);
        let mut breaks = self.buf[..last]
            .iter()
            .enumerate()
            .rev()
            .filter(|&(_, &b)| b == b'\n');
        if let Some((i, _)) = breaks.nth(MAX_BASH_LINES - 1) {
            self.buf.drain(..=i);
            self.cut = true;
        }
    }

    /// The kept text and whether anything was cut.
    fn finish(mut self) -> (String, bool) {
        self.trim_bytes();
        self.trim_lines();
        if self.cut {
            // Do not start in the middle of a UTF-8 character.
            let skip = self.buf.iter().take_while(|&&b| b & 0xc0 == 0x80).count();
            self.buf.drain(..skip);
        }
        let text = match String::from_utf8(self.buf) {
            Ok(text) => text,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        (text, self.cut)
    }
}
