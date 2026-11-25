// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use genai::chat::ToolCall;
use harness_exec::{
    run_worker_io, Executor, Hosts, Request, Response, SandboxConfig, SandboxExecutor, ToolContext,
    ToolDetails,
};
use harness_sandbox::{Options, Sandbox};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        fn_name: name.into(),
        fn_arguments: args,
        thought_signatures: None,
    }
}

#[tokio::test]
async fn local_executor_runs_parallel_calls_and_returns_edit_diff() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "old line\n").unwrap();

    let exec = Executor::Local(ToolContext {
        cwd: dir.path().to_path_buf(),
        ..ToolContext::default()
    });

    let c1 = call("c1", "read", json!({ "path": "a.txt" }));
    let c2 = call(
        "c2",
        "edit",
        json!({ "path": "b.txt", "edits": [{ "oldText": "old line", "newText": "new line" }] }),
    );
    let c3 = call("c3", "bash", json!({ "command": "echo streamed" }));

    let mut streamed = String::new();
    let ((r1, d1), (r2, d2), (r3, d3)) = tokio::join!(
        exec.execute(&c1, |_| {}),
        exec.execute(&c2, |_| {}),
        exec.execute(&c3, |chunk| streamed.push_str(chunk)),
    );

    assert!(r1.content.contains("hello world"), "{}", r1.content);
    assert!(d1.is_none());
    assert!(matches!(d2, Some(ToolDetails::Edit(_))), "{d2:?}");
    assert!(r2.content.contains("\"replacements\":1"), "{}", r2.content);
    assert!(streamed.contains("streamed"), "{streamed:?}");
    assert!(r3.content.contains("streamed"), "{}", r3.content);
    assert!(d3.is_none());
}

#[tokio::test]
async fn worker_runs_parallel_calls_streams_and_cancels_individually() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("edit.txt"), "before\n").unwrap();

    let (mut host_write, worker_read) = tokio::io::duplex(64 * 1024);
    let (worker_write, host_read) = tokio::io::duplex(64 * 1024);
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        ..ToolContext::default()
    };

    let worker =
        tokio::spawn(async move { run_worker_io(worker_read, worker_write, ctx, false).await });
    let mut lines = BufReader::new(host_read).lines();

    // First message is Ready.
    let first = lines.next_line().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Response>(&first).unwrap(),
        Response::Ready
    );

    // Send a slow bash call (id=1), plus fast read (id=2) and edit (id=3) concurrently.
    for req in [
        Request::Call {
            id: 1,
            name: "bash".into(),
            args: json!({ "command": "echo started; sleep 30" }),
        },
        Request::Call {
            id: 2,
            name: "read".into(),
            args: json!({ "path": "note.txt" }),
        },
        Request::Call {
            id: 3,
            name: "edit".into(),
            args: json!({
                "path": "edit.txt",
                "edits": [{ "oldText": "before", "newText": "after" }]
            }),
        },
    ] {
        let mut raw = serde_json::to_vec(&req).unwrap();
        raw.push(b'\n');
        host_write.write_all(&raw).await.unwrap();
    }
    host_write.flush().await.unwrap();

    // id=2 and id=3 must complete quickly even while id=1 is sleeping, and id=1 streams output.
    let start = Instant::now();
    let mut results: HashMap<u64, (String, Option<ToolDetails>)> = HashMap::new();
    let mut saw_stream = false;
    while results.len() < 2 || !saw_stream {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("timed out waiting for parallel responses")
            .unwrap()
            .unwrap();
        match serde_json::from_str::<Response>(&line).unwrap() {
            Response::Output { id: 1, chunk } if chunk.contains("started") => saw_stream = true,
            Response::Result {
                id,
                content,
                details,
            } => {
                results.insert(id, (content, details));
            }
            _ => {}
        }
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "fast calls were blocked by slow bash call"
    );
    assert!(results[&2].0.contains("alpha"), "{}", results[&2].0);
    assert!(matches!(results[&3].1, Some(ToolDetails::Edit(_))));

    // Cancel id=1; worker cancels only id=1 and stays alive for id=4.
    let mut cancel = serde_json::to_vec(&Request::Cancel { id: 1 }).unwrap();
    cancel.push(b'\n');
    host_write.write_all(&cancel).await.unwrap();
    host_write.flush().await.unwrap();

    loop {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("timed out waiting for cancel result")
            .unwrap()
            .unwrap();
        if let Response::Result { id: 1, content, .. } = serde_json::from_str(&line).unwrap() {
            assert!(content.contains("cancelled"), "{content}");
            break;
        }
    }

    // Worker still handles new calls after cancel.
    let mut req4 = serde_json::to_vec(&Request::Call {
        id: 4,
        name: "read".into(),
        args: json!({ "path": "edit.txt" }),
    })
    .unwrap();
    req4.push(b'\n');
    host_write.write_all(&req4).await.unwrap();
    host_write.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match serde_json::from_str::<Response>(&line).unwrap() {
        Response::Result { id: 4, content, .. } => assert!(content.contains("after"), "{content}"),
        other => panic!("unexpected response: {other:?}"),
    }

    // Closing stdin cleanly exits the worker.
    drop(host_write);
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .expect("worker did not exit on EOF")
        .unwrap()
        .unwrap();
}

/// Minimal bash NDJSON mock worker script used to exercise `SandboxExecutor`
/// inside a real `hakoniwa` container without requiring the `kl` binary.
const MOCK_WORKER_SCRIPT: &str = r#"
printf '{"op":"ready"}\n'
while IFS= read -r line; do
  case "$line" in
    *'"op":"cancel"'*)
      # Record cancel marker in container /tmp to verify cancel delivery & /tmp persistence.
      echo cancelled >> /tmp/cancels
      ;;
    *'"name":"stream"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"op":"output","id":%s,"chunk":"part1\\n"}\n' "$id"
      printf '{"op":"output","id":%s,"chunk":"part2\\n"}\n' "$id"
      printf '{"op":"result","id":%s,"content":"{\\"ok\\":true}"}\n' "$id"
      ;;
    *'"name":"parallel"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      val=$(printf '%s' "$line" | sed -n 's/.*"val":"\([^"]*\)".*/\1/p')
      ( sleep 0.05; printf '{"op":"result","id":%s,"content":"{\\"val\\":\\"%s\\"}"}\n' "$id" "$val" ) &
      ;;
    *'"name":"hang"'*)
      # Never responds until cancelled.
      ;;
    *'"name":"cancels"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      count=$(wc -l < /tmp/cancels 2>/dev/null || echo 0)
      printf '{"op":"result","id":%s,"content":"{\\"cancels\\":%s}"}\n' "$id" "$(echo "$count" | tr -d ' ')"
      ;;
    *'"name":"check_isolation"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      secret=$(printf '%s' "$line" | sed -n 's/.*"secret":"\([^"]*\)".*/\1/p')
      has_key="${SECRET_API_KEY:-none}"
      secret_data=$(cat "$secret" 2>/dev/null || echo blocked)
      net_ok=$( (exec 3<>/dev/tcp/127.0.0.1/1) 2>&1 || true )
      printf '{"op":"result","id":%s,"content":"{\\"key\\":\\"%s\\",\\"secret\\":\\"%s\\",\\"net\\":\\"%s\\"}"}\n' \
        "$id" "$has_key" "${secret_data:-empty}" "$(printf '%s' "$net_ok" | tr -d '"\n')"
      ;;
    *'"name":"crash"'*)
      exit 1
      ;;
    *'"name":"ping"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"op":"result","id":%s,"content":"{\\"pong\\":true}"}\n' "$id"
      ;;
  esac
done
"#;

#[tokio::test]
async fn sandbox_executor_streams_multiplexes_isolates_and_respawns() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "top-secret").unwrap();

    let sandbox = Sandbox::with_defaults(&Options {
        write: vec![dir.path().to_path_buf()],
        deny: vec![secret.clone()],
        ..Options::default()
    })
    .no_network();

    let exec = Executor::sandbox(SandboxConfig {
        sandbox,
        program: PathBuf::from("/bin/bash"),
        args: vec!["-c".into(), MOCK_WORKER_SCRIPT.into()],
        cwd: dir.path().to_path_buf(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        hosts: Hosts::default(),
    })
    .expect("sandbox executor should start");

    // 1. Streaming output + result.
    let mut chunks = String::new();
    let (resp, _) = exec
        .execute(&call("s1", "stream", json!({})), |c| chunks.push_str(c))
        .await;
    assert_eq!(chunks, "part1\npart2\n");
    assert_eq!(resp.content, r#"{"ok":true}"#);

    // 2. Concurrent multiplexed calls route by id.
    let c1 = call("p1", "parallel", json!({ "val": "one" }));
    let c2 = call("p2", "parallel", json!({ "val": "two" }));
    let ((r1, _), (r2, _)) = tokio::join!(exec.execute(&c1, |_| {}), exec.execute(&c2, |_| {}));
    assert_eq!(r1.content, r#"{"val":"one"}"#);
    assert_eq!(r2.content, r#"{"val":"two"}"#);

    // 3. Isolation: no API key, denied secret is masked, network is unreachable.
    let (iso, _) = exec
        .execute(
            &call(
                "i1",
                "check_isolation",
                json!({ "secret": secret.to_string_lossy() }),
            ),
            |_| {},
        )
        .await;
    assert!(iso.content.contains(r#""key":"none""#), "{}", iso.content);
    assert!(
        !iso.content.contains("top-secret"),
        "secret leaked: {}",
        iso.content
    );
    assert!(
        iso.content.contains("Network is unreachable"),
        "{}",
        iso.content
    );

    // 4. Dropping an in-flight execute future sends Cancel while keeping /tmp alive.
    let hang = call("h1", "hang", json!({}));
    let timed_out = tokio::time::timeout(Duration::from_millis(50), exec.execute(&hang, |_| {}))
        .await
        .is_err();
    assert!(timed_out);
    let (cancels, _) = exec
        .execute(&call("cn", "cancels", json!({})), |_| {})
        .await;
    assert_eq!(cancels.content, r#"{"cancels":1}"#);

    // 5. Crash recovery: mid-call worker death fails cleanly and next call respawns.
    let (crashed, _) = exec.execute(&call("cr", "crash", json!({})), |_| {}).await;
    assert!(
        crashed.content.contains("exited unexpectedly"),
        "{}",
        crashed.content
    );
    let (recovered, _) = exec.execute(&call("ok", "ping", json!({})), |_| {}).await;
    assert_eq!(recovered.content, r#"{"pong":true}"#);
}

#[tokio::test(start_paused = true)]
async fn worker_that_does_not_stop_background_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::with_defaults(&Options {
        write: vec![dir.path().to_path_buf()],
        ..Options::default()
    })
    .no_network();
    let exec = Executor::sandbox(SandboxConfig {
        sandbox,
        program: PathBuf::from("/bin/bash"),
        args: vec!["-c".into(), MOCK_WORKER_SCRIPT.into()],
        cwd: dir.path().to_path_buf(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        hosts: Hosts::default(),
    })
    .unwrap();
    let cancels = || async {
        let (resp, _) = exec.execute(&call("c", "cancels", json!({})), |_| {}).await;
        resp.content
    };
    let hang = call("h", "hang", json!({}));
    let cancelled = tokio::time::timeout(Duration::from_millis(50), exec.execute(&hang, |_| {}));
    assert!(cancelled.await.is_err());
    assert_eq!(cancels().await, r#"{"cancels":1}"#);

    // The mock never answers, as a worker a command stopped: the wait
    // ends, and the next call gets a new worker, which saw no cancel.
    exec.stop_background(false).await;
    assert_eq!(cancels().await, r#"{"cancels":0}"#);
}

#[tokio::test(start_paused = true)]
async fn call_to_a_worker_that_stopped_reading_can_be_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::with_defaults(&Options {
        write: vec![dir.path().to_path_buf()],
        ..Options::default()
    })
    .no_network();
    let exec = Executor::sandbox(SandboxConfig {
        sandbox,
        program: PathBuf::from("/bin/bash"),
        args: vec![
            "-c".into(),
            r#"printf '{"op":"ready"}\n'; exec sleep 600"#.into(),
        ],
        cwd: dir.path().to_path_buf(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        hosts: Hosts::default(),
    })
    .unwrap();

    // More than the pipe to the worker holds: writing it never ends.
    let big = call("b", "write", json!({ "content": "x".repeat(1 << 20) }));
    let cancelled = tokio::time::timeout(Duration::from_millis(50), exec.execute(&big, |_| {}));
    assert!(cancelled.await.is_err());
    // Dropping the executor ends the write with the worker.
}

#[test]
fn sandbox_executor_fails_fast_when_worker_exits_before_ready() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::with_defaults(&Options {
        write: vec![dir.path().to_path_buf()],
        ..Options::default()
    })
    .no_network();

    let err = SandboxExecutor::new(SandboxConfig {
        sandbox,
        program: PathBuf::from("/bin/bash"),
        args: vec!["-c".into(), "exit 42".into()],
        cwd: dir.path().to_path_buf(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        hosts: Hosts::default(),
    })
    .unwrap_err();
    assert!(err.to_string().contains("42"), "{err}");
}

#[tokio::test]
async fn replaced_worker_program_is_not_restarted() {
    // A command in a writable folder holding the program could rename
    // another one over it, which would run without the worker's limits.
    // Deleted and created again, it may even get the same inode.
    let rename = |program: &std::path::Path| {
        let replacement = program.with_file_name("replacement");
        std::fs::copy("/bin/bash", &replacement).unwrap();
        std::fs::rename(&replacement, program).unwrap();
    };
    let recreate = |program: &std::path::Path| {
        std::fs::remove_file(program).unwrap();
        std::fs::copy("/bin/bash", program).unwrap();
    };
    for replace in [&rename as &dyn Fn(&std::path::Path), &recreate] {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("worker");
        std::fs::copy("/bin/bash", &program).unwrap();
        let sandbox = Sandbox::with_defaults(&Options {
            write: vec![dir.path().to_path_buf()],
            ..Options::default()
        })
        .no_network();
        let exec = Executor::sandbox(SandboxConfig {
            sandbox,
            program: program.clone(),
            args: vec!["-c".into(), MOCK_WORKER_SCRIPT.into()],
            cwd: dir.path().to_path_buf(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            hosts: Hosts::default(),
        })
        .unwrap();
        let (pong, _) = exec.execute(&call("p1", "ping", json!({})), |_| {}).await;
        assert_eq!(pong.content, r#"{"pong":true}"#);

        replace(&program);
        exec.execute(&call("cr", "crash", json!({})), |_| {}).await;

        let (refused, _) = exec.execute(&call("p2", "ping", json!({})), |_| {}).await;
        assert!(refused.content.contains("replaced"), "{}", refused.content);
    }
}

fn group_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
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

/// The pid of the command a `bash` call started in the background.
fn job_pid(content: &str) -> u64 {
    let out: serde_json::Value = serde_json::from_str(content).unwrap();
    out["pid"].as_u64().expect(content)
}

#[tokio::test]
async fn local_executor_keeps_jobs_until_asked() {
    let mut ctx = ToolContext::default();
    ctx.bash.background = true;
    let exec = Executor::Local(ctx);
    let job = call(
        "j",
        "bash",
        json!({ "command": "sleep 30", "background": true }),
    );
    let (resp, _) = exec.execute(&job, |_| {}).await;
    let pid = job_pid(&resp.content);

    assert!(exec.stop_background(true).await);
    assert!(group_alive(pid));
    assert!(exec.stop_background(false).await);
    assert!(eventually(|| !group_alive(pid)).await);
    assert!(!exec.stop_background(false).await);
}

#[tokio::test]
async fn worker_keeps_jobs_until_asked_and_stops_them_on_exit() {
    let (mut host_write, worker_read) = tokio::io::duplex(64 * 1024);
    let (worker_write, host_read) = tokio::io::duplex(64 * 1024);
    let mut ctx = ToolContext::default();
    ctx.bash.background = true;
    let worker = tokio::spawn(run_worker_io(worker_read, worker_write, ctx, false));
    let mut lines = BufReader::new(host_read).lines();
    let mut send = async |req: Request| {
        let mut raw = serde_json::to_vec(&req).unwrap();
        raw.push(b'\n');
        host_write.write_all(&raw).await.unwrap();
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            if let Response::Result { content, .. } = serde_json::from_str(&line).unwrap() {
                return content;
            }
        }
    };
    let job = |id| Request::Call {
        id,
        name: "bash".into(),
        args: json!({ "command": "sleep 30", "background": true }),
    };
    let stop = |id, keep_jobs| Request::StopBackground { id, keep_jobs };

    let kept = job_pid(&send(job(1)).await);
    assert_eq!(send(stop(2, true)).await, r#"{"jobs":true}"#);
    assert!(group_alive(kept));
    assert_eq!(send(stop(3, false)).await, r#"{"jobs":true}"#);
    assert!(eventually(|| !group_alive(kept)).await);

    let left = job_pid(&send(job(4)).await);
    drop(host_write);
    worker.await.unwrap().unwrap();
    assert!(eventually(|| !group_alive(left)).await);
}

/// A worker whose `bash` calls ask the proxy for a tunnel to the
/// `host:port` given as command, and answer with its status line, then
/// with whether connecting there directly works.
const PROXY_WORKER_SCRIPT: &str = r#"
printf '{"op":"ready"}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  target=$(printf '%s' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p')
  exec 3<>"/dev/tcp/127.0.0.1/${https_proxy##*:}"
  printf 'CONNECT %s HTTP/1.1\r\n\r\nping' "$target" >&3
  status=$(head -n1 <&3 | tr -d '\r')
  exec 3>&-
  direct=$( (exec 4<>"/dev/tcp/${target%:*}/${target##*:}") 2>/dev/null && echo open || echo closed)
  printf '{"op":"result","id":%s,"content":"{\\"output\\":\\"%s, direct %s\\"}"}\n' "$id" "$status" "$direct"
done
"#;

#[tokio::test]
async fn commands_reach_listed_hosts_through_the_proxy_only() {
    let dir = tempfile::tempdir().unwrap();
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let sandbox = Sandbox::with_defaults(&Options {
        write: vec![dir.path().to_path_buf()],
        ..Options::default()
    })
    .no_network();
    let exec = Executor::sandbox(SandboxConfig {
        sandbox,
        program: PathBuf::from("/bin/bash"),
        args: vec!["-c".into(), PROXY_WORKER_SCRIPT.into()],
        cwd: dir.path().to_path_buf(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        hosts: Hosts::new(&["127.0.0.1"]).unwrap(),
    })
    .unwrap();

    let target = format!("127.0.0.1:{port}");
    let bash = |target: &str| call("b", "bash", json!({ "command": target }));
    let (allowed, _) = exec.execute(&bash(&target), |_| {}).await;
    let (mut stream, _) = server.accept().unwrap();
    let mut ping = [0; 4];
    std::io::Read::read_exact(&mut stream, &mut ping).unwrap();
    assert_eq!(&ping, b"ping");
    assert_eq!(
        allowed.content,
        r#"{"output":"HTTP/1.1 200 Connection established, direct closed"}"#
    );

    let (refused, _) = exec.execute(&bash("localhost:1"), |_| {}).await;
    let refused: serde_json::Value = serde_json::from_str(&refused.content).unwrap();
    assert_eq!(
        refused,
        json!({
            "output": "HTTP/1.1 403 Forbidden, direct closed",
            "refused_hosts": ["localhost"],
        })
    );
}
