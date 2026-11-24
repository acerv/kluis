// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use harness_exec::{
    BashContext, BashTool, OutputFiles, DEFAULT_BASH_TIMEOUT, MAX_BASH_LINES, MAX_BASH_LOG_BYTES,
    MAX_BASH_OUTPUT,
};

fn call(command: &str, timeout: Option<u64>) -> BashTool {
    BashTool {
        command: command.into(),
        timeout,
        background: false,
    }
}

fn with_hook(hook: impl Fn() -> std::io::Result<()> + Send + Sync + 'static) -> BashContext {
    BashContext {
        pre_exec: Some(Arc::new(hook)),
        ..BashContext::default()
    }
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .unwrap()
        .success()
}

#[test]
fn definition() {
    let tool = BashTool::definition();
    assert_eq!(tool.name.as_ref(), "bash");
    assert_eq!(tool.schema, Some(BashTool::schema()));
}

#[tokio::test]
async fn merged_output_and_exit_code() {
    let out = call("echo out; echo err >&2; echo again; exit 3", None)
        .run()
        .await
        .unwrap();
    assert_eq!(out.output, "out\nerr\nagain\n");
    assert_eq!(out.exit_code, Some(3));
    assert!(!out.truncated);
    assert_eq!(out.output_file, None);
    assert!(!out.timed_out);
}

#[tokio::test]
async fn killed_by_signal() {
    let out = call("kill -9 $$", None).run().await.unwrap();
    assert_eq!(out.exit_code, Some(128 + 9));
}

#[tokio::test]
async fn timeout_keeps_partial_output() {
    let start = Instant::now();
    let out = call("echo before; sleep 30", Some(1)).run().await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(out.timed_out);
    assert_eq!(out.exit_code, None);
    assert_eq!(out.output, "before\n");
}

#[tokio::test]
async fn background_job_keeps_running() {
    let start = Instant::now();
    let out = call("sleep 30 & echo $!", Some(20)).run().await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(!out.timed_out);
    assert_eq!(out.exit_code, Some(0));

    let pid = out.output.trim();
    let running = alive(pid);
    std::process::Command::new("kill")
        .arg(pid)
        .status()
        .unwrap();
    assert!(running, "background job {pid} was killed");
}

#[tokio::test]
async fn reads_background_output_until_quiet() {
    let out = call("(sleep 0.05; echo late) & echo now", Some(20))
        .run()
        .await
        .unwrap();
    assert_eq!(out.output, "now\nlate\n");
}

#[tokio::test]
async fn stdin_is_closed() {
    let out = call("cat", Some(5)).run().await.unwrap();
    assert!(!out.timed_out);
    assert_eq!(out.output, "");
}

#[tokio::test]
async fn output_files_of_the_context_are_removed_with_it() {
    let ctx = BashContext {
        output_files: Some(OutputFiles::default()),
        ..BashContext::default()
    };
    let out = call("head -c 200000 /dev/zero | tr '\\0' a", None)
        .run_with(&ctx.clone())
        .await
        .unwrap();
    let path = out.output_file.expect("spill file path");
    assert!(Path::new(&path).exists());

    drop(ctx);
    assert!(!Path::new(&path).exists());
}

#[tokio::test]
async fn keeps_last_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let out = call("head -c 200000 /dev/zero | tr '\\0' a; echo END", None)
        .run()
        .await
        .unwrap();
    assert!(out.truncated);
    assert_eq!(out.output.len(), MAX_BASH_OUTPUT);
    assert!(out.output.ends_with("aaaEND\n"));

    let path = out.output_file.expect("spill file path");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let full = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(full.len(), 200_004);
    assert!(full.starts_with("aaaa"));
    assert!(full.ends_with("aaaEND\n"));
}

#[tokio::test]
async fn keeps_last_lines() {
    let out = call("seq 5000", None).run().await.unwrap();
    assert!(out.truncated);
    assert_eq!(out.output.lines().count(), MAX_BASH_LINES);
    assert!(out
        .output
        .starts_with(&format!("{}\n", 5000 - MAX_BASH_LINES + 1)));
    assert!(out.output.ends_with("\n5000\n"));

    let path = out.output_file.expect("spill file path");
    let full = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(full.lines().count(), 5000);
    assert!(full.starts_with("1\n"));
    assert!(full.ends_with("\n5000\n"));
}

#[tokio::test]
async fn cut_does_not_split_characters() {
    // "é" is two bytes; an odd total makes the cut land inside one.
    let out = call(
        "printf 'x'; for i in $(seq 40000); do printf 'é'; done",
        None,
    )
    .run()
    .await
    .unwrap();
    assert!(out.truncated);
    assert!(
        out.output.chars().all(|c| c == 'é'),
        "starts with a fragment"
    );

    let path = out.output_file.expect("spill file path");
    let full = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(full.chars().count(), 40_001);
    assert!(full.starts_with('x'));
    assert!(full[1..].chars().all(|c| c == 'é'));
}

#[tokio::test]
async fn spill_file_keeps_full_output_when_both_bytes_and_lines_truncate() {
    let out = call("seq 30000", None).run().await.unwrap();
    assert!(out.truncated);
    assert_eq!(out.output.lines().count(), MAX_BASH_LINES);
    assert!(out
        .output
        .starts_with(&format!("{}\n", 30_000 - MAX_BASH_LINES + 1)));
    assert!(out.output.ends_with("\n30000\n"));

    let path = out.output_file.expect("spill file path");
    let full = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let expected: String = (1..=30_000).map(|i| format!("{i}\n")).collect();
    assert_eq!(full, expected);
}

#[tokio::test]
async fn spill_file_is_capped_at_max_bash_log_bytes() {
    let total = MAX_BASH_LOG_BYTES + 100_000;
    let out = call(
        &format!("head -c {total} /dev/zero | tr '\\0' a; echo END"),
        None,
    )
    .run()
    .await
    .unwrap();
    assert!(out.truncated);
    assert!(out.output.ends_with("aaaEND\n"));

    let path = out.output_file.expect("spill file path");
    let len = std::fs::metadata(&path).unwrap().len();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(len, MAX_BASH_LOG_BYTES);
}

#[tokio::test]
async fn strips_colors() {
    let out = call(
        r"printf '\033[1;31mred\033[0m \033]0;title\007ok \033]8;;x\033\\link\033]8;;\033\\\n'",
        None,
    )
    .run()
    .await
    .unwrap();
    assert_eq!(out.output, "red ok link\n");
}

#[tokio::test]
async fn strips_colors_split_across_reads() {
    let out = call(r"printf '\033[3'; sleep 0.2; printf '1mx\n'", None)
        .run()
        .await
        .unwrap();
    assert_eq!(out.output, "x\n");
}

#[tokio::test]
async fn strips_control_characters() {
    let out = call(r"printf 'a\tb\001\002c\b\r\n'", None)
        .run()
        .await
        .unwrap();
    assert_eq!(out.output, "a\tbc\r\n");
}

#[tokio::test]
async fn zero_timeout_fails() {
    let err = call("true", Some(0)).run().await.unwrap_err();
    assert!(err.message.contains("timeout"), "{err}");
}

#[tokio::test]
async fn streams_cleaned_output_as_it_arrives() {
    let mut chunks = Vec::new();
    let out = call(r"printf '\033[32mstep1\033[0m\n'; echo err1 >&2", None)
        .run_in_streaming(
            std::path::Path::new("."),
            &BashContext::default(),
            |chunk| {
                chunks.push(chunk.to_owned());
            },
        )
        .await
        .unwrap();

    assert_eq!(out.output, "step1\nerr1\n");
    assert_eq!(chunks.concat(), "step1\nerr1\n");
}

#[tokio::test]
async fn huge_timeout_means_no_limit() {
    let out = call("echo hi", Some(u64::MAX)).run().await.unwrap();
    assert_eq!(out.output, "hi\n");
}

// Paused time jumps to the next timer when the runtime is idle, so this
// does not wait for the default timeout.
#[tokio::test(start_paused = true)]
async fn default_timeout_applies() {
    let start = tokio::time::Instant::now();
    let out = call("sleep 30", None).run().await.unwrap();
    assert!(out.timed_out);
    assert_eq!(start.elapsed().as_secs(), DEFAULT_BASH_TIMEOUT);
}

#[tokio::test]
async fn command_runs_unchanged() {
    let out = call("echo $LINENO", None).run().await.unwrap();
    assert_eq!(out.output, "1\n");

    let out = call("echo 'unterminated", None).run().await.unwrap();
    assert!(out.output.contains("line 1"), "{}", out.output);
    assert_ne!(out.exit_code, Some(0));
}

#[tokio::test]
async fn endless_background_writer_does_not_block() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let command = format!(
        "(while :; do echo x; sleep 0.01; done) & echo $! > {}",
        pidfile.display()
    );

    let start = Instant::now();
    let out = call(&command, None).run().await.unwrap();
    let elapsed = start.elapsed();
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    std::process::Command::new("kill")
        .arg(pid.trim())
        .status()
        .unwrap();

    assert!(elapsed < Duration::from_secs(6), "took {elapsed:?}");
    assert_eq!(out.exit_code, Some(0));
    assert!(!out.timed_out);
}

#[tokio::test]
async fn dropped_run_kills_command() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let command = format!("sleep 30 & echo $! > {}; wait", pidfile.display());

    let run = call(&command, None).run();
    assert!(tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .is_err());

    let pid = std::fs::read_to_string(&pidfile).unwrap();
    let pid = pid.trim();
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(pid), "sleep {pid} survived");
}

#[tokio::test]
async fn failing_hook_stops_command() {
    let ctx = with_hook(|| {
        Err(std::io::Error::from_raw_os_error(
            nix::errno::Errno::EPERM as i32,
        ))
    });

    let err = call("echo never", None).run_with(&ctx).await.unwrap_err();
    assert!(err.message.contains("Operation not permitted"), "{err}");
}

#[tokio::test]
async fn env_replaces_inherited_environment() {
    let ctx = BashContext {
        env: Some(vec![("HARNESS_TEST".into(), "value".into())]),
        ..BashContext::default()
    };

    // Cargo gives tests CARGO_* variables; the command must not see them.
    let script = "echo \"$HARNESS_TEST ${CARGO_MANIFEST_DIR-unset}\"";
    let out = call(script, None).run_with(&ctx).await.unwrap();
    assert_eq!(out.output, "value unset\n");
}

#[tokio::test]
async fn environment_inherited_by_default() {
    let out = call("echo \"${CARGO_MANIFEST_DIR-unset}\"", None)
        .run()
        .await
        .unwrap();
    assert_eq!(out.output, format!("{}\n", env!("CARGO_MANIFEST_DIR")));
}

#[tokio::test]
async fn runs_in_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let out = call("pwd", None)
        .run_in(dir.path(), &BashContext::default())
        .await
        .unwrap();
    let pwd = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(out.output, format!("{}\n", pwd.display()));
}

#[tokio::test]
async fn missing_cwd_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone");
    let err = call("true", None)
        .run_in(&gone, &BashContext::default())
        .await
        .unwrap_err();
    assert!(
        err.message.starts_with(&gone.display().to_string()),
        "{err}"
    );
    assert!(err.message.contains("No such file or directory"), "{err}");
}

fn job(command: &str) -> BashTool {
    BashTool {
        background: true,
        ..call(command, None)
    }
}

/// Wait up to 5 seconds for `done`.
async fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > Duration::from_secs(5) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

fn group_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .status()
        .unwrap()
        .success()
}

#[tokio::test]
async fn background_command_returns_at_once_and_logs_its_output() {
    let ctx = BashContext::default();
    let start = Instant::now();
    let out = job("printf '\\033[31mred\\033[0m\\n'; sleep 30")
        .start_in(Path::new("."), &ctx)
        .await
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(ctx.jobs.pids(), [out.pid]);
    let file = Path::new(&out.output_file);
    assert!(eventually(|| std::fs::read_to_string(file).unwrap() == "red\n").await);

    ctx.jobs.stop();
    assert!(ctx.jobs.pids().is_empty());
    assert!(eventually(|| !group_alive(out.pid)).await);
    std::fs::remove_file(file).unwrap();
}

#[tokio::test]
async fn background_command_that_ends_leaves_the_jobs() {
    let ctx = BashContext::default();
    let out = job("echo done")
        .start_in(Path::new("."), &ctx)
        .await
        .unwrap();
    assert!(eventually(|| ctx.jobs.pids().is_empty()).await);
    let file = Path::new(&out.output_file);
    assert_eq!(std::fs::read_to_string(file).unwrap(), "done\n");
    std::fs::remove_file(file).unwrap();
}

#[tokio::test]
async fn background_commands_stop_with_the_last_context() {
    let ctx = BashContext {
        output_files: Some(OutputFiles::default()),
        ..BashContext::default()
    };
    let out = job("sleep 30 & sleep 30")
        .start_in(Path::new("."), &ctx)
        .await
        .unwrap();
    assert!(group_alive(out.pid));
    drop(ctx.clone());
    assert!(group_alive(out.pid));
    drop(ctx);
    assert!(eventually(|| !group_alive(out.pid)).await);
    assert!(!Path::new(&out.output_file).exists());
}

#[tokio::test]
async fn background_output_file_restarts_when_full() {
    let ctx = BashContext {
        output_files: Some(OutputFiles::default()),
        ..BashContext::default()
    };
    let size = MAX_BASH_LOG_BYTES + 100;
    let out = job(&format!(
        "head -c {size} /dev/zero | tr '\\0' a; echo; echo end"
    ))
    .start_in(Path::new("."), &ctx)
    .await
    .unwrap();
    assert!(eventually(|| ctx.jobs.pids().is_empty()).await);
    let text = std::fs::read_to_string(&out.output_file).unwrap();
    assert!(
        text.starts_with("[earlier output dropped]\n"),
        "{}",
        &text[..40]
    );
    assert!(text.ends_with("a\nend\n"));
    assert!(text.len() as u64 <= MAX_BASH_LOG_BYTES);
}

#[test]
fn background_is_left_out_unless_set() {
    let json = serde_json::to_value(call("true", None)).unwrap();
    assert_eq!(json, serde_json::json!({ "command": "true" }));
    let tool: BashTool =
        serde_json::from_value(serde_json::json!({ "command": "x", "background": true })).unwrap();
    assert!(tool.background);
}
