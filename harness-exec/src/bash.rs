// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use genai::chat::Tool;
use nix::unistd::setsid;
use scopeguard::ScopeGuard;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tempfile::{Builder, TempPath};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;
use tokio::process::{Child, Command};
use tokio::time::{sleep_until, Instant};

use crate::error::ToolError;
use crate::lock;

mod jobs;
mod output;

use jobs::kill_group;
pub use jobs::Jobs;
use output::{Filter, Tail};

/// Name of the `bash` tool.
pub const BASH_TOOL: &str = "bash";

/// What the `bash` tool is not for, ending its description.
const BASH_RULES: &str = "

IMPORTANT: Avoid using this tool to run `find`, `grep`, `cat`, `head`, \
`tail`, `sed`, `awk`, or `echo` commands, unless explicitly instructed or \
when these commands are truly necessary for the task. Instead, always \
prefer using the dedicated tools for these commands:
- File search: Use the find tool (NOT the find or ls commands)
- Content search: Use the grep tool (NOT the grep or rg commands)
- Read files: Use the read tool (NOT cat/head/tail)
- Edit files: Use the edit tool (NOT sed/awk)
- Write files: Use the write tool (NOT echo >/cat <<EOF)
- Communication: Output text directly (NOT echo/printf)

Try to maintain your current working directory throughout the session by \
using absolute paths and avoiding usage of `cd`.";

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
        description.push_str(BASH_RULES);
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
        ctx.jobs.watch(pid, child, pipe, file);
        Ok(JobOutput { pid, output_file })
    }
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
