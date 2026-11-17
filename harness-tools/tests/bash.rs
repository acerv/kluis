// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::sync::Arc;
use std::time::{Duration, Instant};

use harness_tools::{BashContext, BashTool, MAX_BASH_LINES, MAX_BASH_OUTPUT};

fn call(command: &str, timeout: Option<u64>) -> BashTool {
    BashTool {
        command: command.into(),
        timeout,
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
async fn keeps_last_bytes() {
    let out = call("head -c 200000 /dev/zero | tr '\\0' a; echo END", None)
        .run()
        .await
        .unwrap();
    assert!(out.truncated);
    assert_eq!(out.output.len(), MAX_BASH_OUTPUT);
    assert!(out.output.ends_with("aaaEND\n"));
}

#[tokio::test]
async fn keeps_last_lines() {
    let out = call("seq 5000", None).run().await.unwrap();
    assert!(out.truncated);
    assert_eq!(out.output.lines().count(), MAX_BASH_LINES);
    assert!(out.output.starts_with("3001\n"));
    assert!(out.output.ends_with("\n5000\n"));
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
async fn huge_timeout_means_no_limit() {
    let out = call("echo hi", Some(u64::MAX)).run().await.unwrap();
    assert_eq!(out.output, "hi\n");
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
    let ctx = with_hook(|| Err(std::io::Error::from_raw_os_error(libc::EPERM)));

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
