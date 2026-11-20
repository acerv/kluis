// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

mod common;

use std::path::Path;
use std::time::Duration;

use common::{text, thinking, tool_calls, truncated_tool_calls, MockModel, Reply};
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatRole};
use harness_core::tools::ToolContext;
use harness_core::{
    find_agents_files, system_prompt, Agent, Event, RetryPolicy, Thinking, ToolDetails,
    AGENTS_FILE, MAX_AGENTS_BYTES,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

fn assert_send<T: Send>(_: T) {}

/// An agent talking to `model`, with tools working in `cwd`.
fn agent(model: &MockModel, cwd: &Path) -> Agent {
    Agent::with_client(model.client(), "test-model").with_tool_context(ToolContext {
        cwd: cwd.to_owned(),
        ..ToolContext::default()
    })
}

fn roles(agent: &Agent) -> Vec<ChatRole> {
    agent.history().iter().map(|m| m.role.clone()).collect()
}

/// The messages of a request sent to the model with `role`.
fn sent(request: &Value, role: &str) -> Vec<Value> {
    let messages = request["messages"].as_array().unwrap();
    messages
        .iter()
        .filter(|m| m["role"] == role)
        .cloned()
        .collect()
}

/// Run a prompt, failing the test if it hangs.
async fn prompt(agent: &mut Agent, text: &str) -> Result<String, harness_core::Error> {
    tokio::time::timeout(Duration::from_secs(10), agent.prompt(text, |_| {}))
        .await
        .expect("prompt hangs")
}

/// Run a prompt and drop it once the model calls the tool `call_id`, while
/// that call is about to run.
async fn cancel_at_call(agent: &mut Agent, text: &str, call_id: &str) {
    let reached = Notify::new();
    let run = agent.prompt(text, |event| {
        if matches!(event, Event::ToolCall(call) if call.call_id == call_id) {
            reached.notify_one();
        }
    });
    tokio::select! {
        out = run => panic!("prompt ended before the cancel: {out:?}"),
        _ = reached.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("call never made"),
    }
}

/// The tool result answering `call_id` in the history.
fn result_of(message: &ChatMessage, call_id: &str) -> Option<String> {
    let responses = message.content.tool_responses();
    let resp = responses.iter().find(|r| r.call_id == call_id)?;
    Some(resp.content.clone())
}

/// A UI runs the agent on its own task, which needs a `Send` future.
#[test]
fn prompt_future_is_send() {
    let mut agent = Agent::new("deepseek-chat").unwrap();
    assert_send(agent.prompt("hi", |_| {}));
}

#[test]
fn history_starts_empty() {
    let agent = Agent::new("deepseek-chat").unwrap();
    assert!(agent.history().is_empty());
    assert_eq!(agent.model(), "deepseek-chat");
}

#[tokio::test]
async fn set_model_and_clear_history() {
    let model = MockModel::start(vec![text("first"), text("second")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    assert_eq!(prompt(&mut agent, "hi").await.unwrap(), "first");
    assert_eq!(agent.history().len(), 2);

    agent.set_model("other-model");
    assert_eq!(agent.model(), "other-model");
    agent.clear();
    assert!(agent.history().is_empty());

    assert_eq!(prompt(&mut agent, "again").await.unwrap(), "second");
    let reqs = model.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1]["model"], "other-model");
    // Only the fresh user message was sent after clear().
    assert_eq!(sent(&reqs[1], "user").len(), 1);
    assert_eq!(sent(&reqs[1], "assistant").len(), 0);
}

#[tokio::test]
async fn thinking_effort_is_sent_in_requests() {
    let model = MockModel::start(vec![text("a"), text("b"), text("c")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    // `Default` omits `reasoning_effort`.
    assert_eq!(agent.thinking(), Thinking::Default);
    prompt(&mut agent, "first").await.unwrap();
    assert!(model.requests()[0].get("reasoning_effort").is_none());

    // Setting `High` forwards the reasoning effort keyword; `Default` omits it again.
    agent.set_thinking(Thinking::High);
    assert_eq!(agent.thinking(), Thinking::High);
    prompt(&mut agent, "second").await.unwrap();
    assert_eq!(model.requests()[1]["reasoning_effort"], "high");

    agent.set_thinking(Thinking::Default);
    prompt(&mut agent, "third").await.unwrap();
    assert!(model.requests()[2].get("reasoning_effort").is_none());

    // Parsing and cycling helpers.
    assert_eq!("MEDIUM".parse::<Thinking>(), Ok(Thinking::Medium));
    assert_eq!("default".parse::<Thinking>(), Ok(Thinking::Default));
    assert!("turbo".parse::<Thinking>().is_err());
    assert_eq!(Thinking::Default.next(), Thinking::Low);
    assert_eq!(Thinking::High.next(), Thinking::Default);
}

#[tokio::test]
async fn text_answer() {
    let model = MockModel::start(vec![text("done")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    assert_eq!(prompt(&mut agent, "hi").await.unwrap(), "done");
    assert_eq!(roles(&agent), [ChatRole::User, ChatRole::Assistant]);
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test]
async fn tool_call_round_trip() {
    let call = ("call_1", "bash", json!({ "command": "echo hi" }));
    let model = MockModel::start(vec![tool_calls(&[call]), text("ok")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let mut events = Vec::new();
    let answer = agent
        .prompt("run it", |event| {
            events.push(match event {
                Event::TurnStart => "start".to_owned(),
                Event::TurnEnd(usage) => format!("end {:?}", usage.and_then(|u| u.total_tokens)),
                Event::ReasoningDelta(text) => format!("reason {text}"),
                Event::TextDelta(text) => format!("delta {text}"),
                Event::Text(text) => format!("text {text}"),
                Event::ToolCall(call) => format!("call {}", call.fn_name),
                Event::ToolOutput(chunk) => format!("out {}", chunk.trim_end()),
                Event::ToolResult(resp, _) => format!("result {}", resp.call_id),
                Event::Retry { reason, .. } => format!("retry {reason}"),
            })
        })
        .await
        .unwrap();

    assert_eq!(answer, "ok");
    assert_eq!(
        events,
        [
            "start",
            "end Some(15)",
            "call bash",
            "out hi",
            "result call_1",
            "start",
            "delta ok",
            "text ok",
            "end Some(15)"
        ]
    );
    assert_eq!(
        roles(&agent),
        [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Assistant
        ]
    );
    let results = sent(&model.requests()[1], "tool");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call_1");
    let output: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(output["output"], "hi\n");
}

#[tokio::test]
async fn truncated_tool_calls_fail_without_running_and_ask_model_to_reissue() {
    let dir = tempfile::tempdir().unwrap();
    let truncated = truncated_tool_calls(&[
        (
            "call_1",
            "write",
            r#"{"path":"first.txt","content":"nope"}"#,
        ),
        ("call_2", "write", r#"{"path":"second.txt","content":"cut"#),
    ]);
    let reissue = tool_calls(&[(
        "call_3",
        "write",
        json!({ "path": "final.txt", "content": "ok" }),
    )]);
    let model = MockModel::start(vec![truncated, reissue, text("done")]).await;
    let mut agent = agent(&model, dir.path());

    let mut tool_errors = Vec::new();
    let answer = agent
        .prompt("write files", |event| {
            if let Event::ToolResult(resp, _) = event {
                let v: Value = serde_json::from_str(&resp.content).unwrap();
                if let Some(err) = v["error"].as_str() {
                    tool_errors.push((resp.call_id.clone(), err.to_owned()));
                }
            }
        })
        .await
        .unwrap();

    assert_eq!(answer, "done");
    // Neither the complete nor the partial call from the truncated turn ran.
    assert!(!dir.path().join("first.txt").exists());
    assert!(!dir.path().join("second.txt").exists());
    // The re-issued call on the following turn succeeded.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("final.txt")).unwrap(),
        "ok"
    );

    assert_eq!(tool_errors.len(), 2);
    assert_eq!(tool_errors[0].0, "call_1");
    assert_eq!(tool_errors[1].0, "call_2");
    for (_, err) in &tool_errors {
        assert!(err.contains("truncated"), "{err}");
        assert!(err.contains("re-issue"), "{err}");
    }

    // Both failed calls were sent back with valid object arguments and matching tool results.
    let reqs = model.requests();
    assert_eq!(reqs.len(), 3);
    let assistant_calls = sent(&reqs[1], "assistant")[0]["tool_calls"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(assistant_calls.len(), 2);
    for tc in &assistant_calls {
        let parsed: Value =
            serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
        assert!(parsed.is_object(), "expected JSON object args: {parsed}");
    }
    let results = sent(&reqs[1], "tool");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["tool_call_id"], "call_1");
    assert_eq!(results[1]["tool_call_id"], "call_2");
}

#[tokio::test]
async fn runs_tool_turns_until_the_model_answers() {
    // There is no turn budget: the agent runs every round the model asks for.
    let rounds = 12;
    let call = ("call_1", "bash", json!({ "command": "true" }));
    let mut replies: Vec<Reply> = (0..rounds)
        .map(|_| tool_calls(std::slice::from_ref(&call)))
        .collect();
    replies.push(text("done"));
    let model = MockModel::start(replies).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    assert_eq!(prompt(&mut agent, "loop").await.unwrap(), "done");
    assert_eq!(model.requests().len(), rounds + 1);
    assert_eq!(agent.history().len(), 1 + rounds * 2 + 1);
    assert_eq!(agent.history().last().unwrap().role, ChatRole::Assistant);
}

#[tokio::test]
async fn cancel_during_a_tool_answers_its_call() {
    let call = ("call_1", "bash", json!({ "command": "sleep 30" }));
    let model = MockModel::start(vec![tool_calls(&[call]), text("after")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    cancel_at_call(&mut agent, "run it", "call_1").await;

    assert_eq!(
        roles(&agent),
        [ChatRole::User, ChatRole::Assistant, ChatRole::Tool]
    );
    let result = result_of(&agent.history()[2], "call_1").unwrap();
    assert!(result.contains("cancelled"), "{result}");

    // The next request answers every call, as providers require.
    assert_eq!(prompt(&mut agent, "next").await.unwrap(), "after");
    let results = sent(&model.requests()[1], "tool");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call_1");
}

#[tokio::test]
async fn cancel_keeps_finished_results() {
    let calls = [
        ("call_1", "bash", json!({ "command": "echo first" })),
        ("call_2", "bash", json!({ "command": "sleep 30" })),
    ];
    let model = MockModel::start(vec![tool_calls(&calls)]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    cancel_at_call(&mut agent, "run both", "call_2").await;

    assert_eq!(
        roles(&agent),
        [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Tool
        ]
    );
    let first = result_of(&agent.history()[2], "call_1").unwrap();
    assert!(first.contains("first"), "{first}");
    let second = result_of(&agent.history()[3], "call_2").unwrap();
    assert!(second.contains("cancelled"), "{second}");
}

#[tokio::test]
async fn cancel_while_waiting_keeps_the_user_message() {
    let model = MockModel::start(vec![Reply::Hang, text("second")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let run = agent.prompt("first", |_| {});
    let asked = async {
        while model.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        out = run => panic!("prompt ended before the cancel: {out:?}"),
        _ = asked => {}
    }

    assert_eq!(roles(&agent), [ChatRole::User]);
    assert_eq!(prompt(&mut agent, "again").await.unwrap(), "second");
    assert_eq!(sent(&model.requests()[1], "user").len(), 2);
}

#[tokio::test]
async fn reasoning_streams_before_text() {
    let model = MockModel::start(vec![thinking("let me think", text("answer"))]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let mut events = Vec::new();
    agent
        .prompt("hi", |event| match event {
            Event::ReasoningDelta(text) => events.push(format!("reason {text}")),
            Event::TextDelta(text) => events.push(format!("delta {text}")),
            _ => {}
        })
        .await
        .unwrap();

    assert_eq!(
        events,
        [
            "reason let",
            "reason  me",
            "reason  th",
            "reason ink",
            "delta ans",
            "delta wer"
        ]
    );
}

#[tokio::test]
async fn text_streams_in_pieces() {
    let model = MockModel::start(vec![text("hello world")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let mut events = Vec::new();
    agent
        .prompt("hi", |event| match event {
            Event::TextDelta(text) => events.push(format!("delta {text}")),
            Event::Text(text) => events.push(format!("text {text}")),
            _ => {}
        })
        .await
        .unwrap();

    assert_eq!(
        events,
        [
            "delta hel",
            "delta lo ",
            "delta wor",
            "delta ld",
            "text hello world"
        ]
    );
    assert_eq!(agent.history()[1].content.first_text(), Some("hello world"));
}

#[tokio::test]
async fn cancel_while_streaming_drops_the_partial_reply() {
    let model =
        MockModel::start(vec![Reply::Partial("half an answer".into()), text("again")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path());

    let streaming = Notify::new();
    let run = agent.prompt("first", |event| {
        if let Event::TextDelta(_) = event {
            streaming.notify_one();
        }
    });
    tokio::select! {
        out = run => panic!("prompt ended before the cancel: {out:?}"),
        _ = streaming.notified() => {}
    }

    assert_eq!(roles(&agent), [ChatRole::User]);
    assert_eq!(prompt(&mut agent, "next").await.unwrap(), "again");
    assert!(sent(&model.requests()[1], "assistant").is_empty());
}

#[tokio::test]
async fn broken_stream_is_an_error() {
    let model = MockModel::start(vec![Reply::Broken("half an answer".into()), text("again")]).await;
    let dir = tempfile::tempdir().unwrap();
    let no_retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    let mut agent = agent(&model, dir.path()).with_retry(no_retry);

    let mut ended = false;
    let run = agent.prompt("first", |event| ended |= matches!(event, Event::TurnEnd(_)));
    let err = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("prompt hangs")
        .unwrap_err();
    assert!(err.to_string().contains("ended early"), "{err}");
    assert!(!ended, "a cut turn must not end");
    assert_eq!(roles(&agent), [ChatRole::User]);
    assert_eq!(prompt(&mut agent, "next").await.unwrap(), "again");
}

/// Run a tool round trip where the model reasons first, talking to the
/// fake server as `kind`, and return the assistant message sent back.
async fn reasoning_sent_back(kind: AdapterKind) -> Value {
    let call = ("call_1", "bash", json!({ "command": "true" }));
    let replies = vec![thinking("plan it", tool_calls(&[call])), text("done")];
    let model = MockModel::start(replies).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent =
        Agent::with_client(model.client_as(kind), "test-model").with_tool_context(ToolContext {
            cwd: dir.path().to_owned(),
            ..ToolContext::default()
        });

    assert_eq!(prompt(&mut agent, "go").await.unwrap(), "done");
    let mut sent = sent(&model.requests()[1], "assistant");
    assert_eq!(sent.len(), 1);
    sent.remove(0)
}

#[tokio::test]
async fn reasoning_goes_back_to_providers_needing_it() {
    for kind in [
        AdapterKind::DeepSeek,
        AdapterKind::Kimi,
        AdapterKind::Moonshot,
    ] {
        let message = reasoning_sent_back(kind).await;
        assert_eq!(
            message["reasoning_content"], "plan it",
            "{kind:?}: {message}"
        );
        // The reasoning is not part of the text.
        assert_eq!(message["content"], "", "{kind:?}: {message}");
    }
}

#[tokio::test]
async fn reasoning_stays_out_for_other_providers() {
    let message = reasoning_sent_back(AdapterKind::OpenAI).await;
    assert!(message.get("reasoning_content").is_none(), "{message}");
}

#[tokio::test]
async fn edit_tool_emits_diff_details_without_sending_to_model() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "hello\n").unwrap();

    let call = (
        "call_1",
        "edit",
        json!({ "path": "a.txt", "edits": [{ "oldText": "hello", "newText": "world" }] }),
    );
    let model = MockModel::start(vec![tool_calls(&[call]), text("done")]).await;
    let mut agent = agent(&model, dir.path());

    let mut seen_details = Vec::new();
    let answer = agent
        .prompt("change it", |event| {
            if let Event::ToolResult(_, details) = event {
                seen_details.push(details.cloned());
            }
        })
        .await
        .unwrap();

    assert_eq!(answer, "done");
    assert_eq!(seen_details.len(), 1);
    let Some(ToolDetails::Edit(diff)) = &seen_details[0] else {
        panic!("expected edit diff details, got {:?}", seen_details[0]);
    };
    assert_eq!(diff.first_changed_line, Some(1));
    assert_eq!(diff.to_string(), "@@ -1 +1 @@\n-hello\n+world\n");

    // The model only receives the compact JSON output, not the UI diff.
    let tool_msgs = sent(&model.requests()[1], "tool");
    assert_eq!(tool_msgs.len(), 1);
    let payload: Value = serde_json::from_str(tool_msgs[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload, json!({ "replacements": 1 }));
}

#[tokio::test]
async fn system_prompt_sent_and_preserved_across_clear() {
    let model = MockModel::start(vec![text("one"), text("two")]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&model, dir.path()).with_system("be concise");
    assert_eq!(agent.system(), Some("be concise"));

    assert_eq!(prompt(&mut agent, "first").await.unwrap(), "one");
    agent.clear();
    assert_eq!(agent.system(), Some("be concise"));
    assert_eq!(prompt(&mut agent, "second").await.unwrap(), "two");

    let reqs = model.requests();
    for req in &reqs {
        let sys = sent(req, "system");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["content"], "be concise");
    }
}

#[test]
fn find_agents_files_orders_global_repo_root_and_subdir_and_deduplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let global_dir = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let sub = repo.join("crates").join("pkg");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&sub).unwrap();

    // An AGENTS.md above the git root must NOT be picked up.
    std::fs::write(tmp.path().join(AGENTS_FILE), "outside repo").unwrap();
    let global = global_dir.join(AGENTS_FILE);
    let root_agents = repo.join(AGENTS_FILE);
    let sub_agents = sub.join(AGENTS_FILE);
    std::fs::write(&global, "global rules").unwrap();
    std::fs::write(&root_agents, "repo rules").unwrap();
    std::fs::write(&sub_agents, "subdir rules").unwrap();

    let found = find_agents_files(&sub, Some(&global));
    assert_eq!(found, vec![global.clone(), root_agents, sub_agents]);

    // When cwd is the global directory itself, the file is only listed once.
    let dedup = find_agents_files(&global_dir, Some(&global));
    assert_eq!(dedup, vec![global]);

    // Outside a git repo, only cwd/AGENTS.md is checked (not parent folders).
    let non_git_parent = tmp.path().join("plain");
    let non_git_child = non_git_parent.join("child");
    std::fs::create_dir_all(&non_git_child).unwrap();
    std::fs::write(non_git_parent.join(AGENTS_FILE), "parent").unwrap();
    assert!(find_agents_files(&non_git_child, None).is_empty());
}

#[test]
fn symlinked_project_agents_file_is_ignored() {
    // The sandboxed model can write the project folder, and could link
    // AGENTS.md to a file the sandbox hides, for the host to read.
    let tmp = tempfile::tempdir().unwrap();
    let hidden = tmp.path().join("auth.toml");
    std::fs::write(&hidden, "api_key = \"sk-secret\"").unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    std::os::unix::fs::symlink(&hidden, cwd.join(AGENTS_FILE)).unwrap();

    assert!(find_agents_files(&cwd, None).is_empty());
    // Not even when handed the path, as if swapped in after it was found.
    let prompt = system_prompt(&cwd, &[cwd.join(AGENTS_FILE)]);
    assert!(!prompt.contains("sk-secret"), "{prompt}");
}

#[test]
fn symlinked_global_agents_file_is_followed() {
    // Only the user writes the global folder.
    let tmp = tempfile::tempdir().unwrap();
    let rules = tmp.path().join("rules.md");
    std::fs::write(&rules, "global rules").unwrap();
    let global = tmp.path().join(AGENTS_FILE);
    std::os::unix::fs::symlink(&rules, &global).unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir(&cwd).unwrap();

    let found = find_agents_files(&cwd, Some(&global));
    assert_eq!(found, [std::fs::canonicalize(&rules).unwrap()]);
    assert!(system_prompt(&cwd, &found).contains("global rules"));
}

#[test]
fn system_prompt_includes_context_and_truncates_large_agents_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let empty = cwd.join("empty.md");
    let rules = cwd.join(AGENTS_FILE);
    let huge = cwd.join("huge.md");
    std::fs::write(&empty, "   \n\t\n").unwrap();
    std::fs::write(&rules, "- format code\n- run clippy\n").unwrap();
    // Place a 3-byte UTF-8 character straddling the MAX_AGENTS_BYTES boundary.
    let mut big = "a".repeat(MAX_AGENTS_BYTES - 1);
    big.push('…');
    big.push_str("tail");
    std::fs::write(&huge, big).unwrap();

    let missing = cwd.join("missing.md");
    let text = system_prompt(cwd, &[missing, empty, rules.clone(), huge.clone()]);

    assert!(text.contains(&cwd.display().to_string()), "{text}");
    assert!(text.contains("OS: linux"), "{text}");
    assert!(text.contains("## Instructions"), "{text}");
    assert!(text.contains(&rules.display().to_string()), "{text}");
    assert!(text.contains("- format code"), "{text}");
    assert!(text.contains(&huge.display().to_string()), "{text}");
    assert!(text.contains("… (truncated)"), "{text}");
    assert!(!text.contains("tail"), "{text}");
    assert!(!text.contains("empty.md"), "{text}");
    assert!(!text.contains("missing.md"), "{text}");
}

#[tokio::test]
async fn parallel_read_only_calls_preserve_paired_events_and_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();

    let calls = [
        ("c1", "read", json!({ "path": "a.txt" })),
        ("c2", "read", json!({ "path": "b.txt" })),
        (
            "c3",
            "write",
            json!({ "path": "c.txt", "content": "gamma\n" }),
        ),
        ("c4", "read", json!({ "path": "c.txt" })),
    ];
    let model = MockModel::start(vec![tool_calls(&calls), text("all done")]).await;
    let mut agent = Agent::with_client(model.client(), "test-model").with_executor(
        harness_core::tools::Executor::Local(ToolContext {
            cwd: dir.path().to_owned(),
            ..ToolContext::default()
        }),
    );

    let mut events = Vec::new();
    let answer = agent
        .prompt("inspect and write", |ev| match ev {
            Event::ToolCall(c) => events.push(format!("call:{}", c.call_id)),
            Event::ToolResult(r, _) => events.push(format!("res:{}", r.call_id)),
            _ => {}
        })
        .await
        .unwrap();

    assert_eq!(answer, "all done");
    assert_eq!(
        events,
        vec!["call:c1", "res:c1", "call:c2", "res:c2", "call:c3", "res:c3", "call:c4", "res:c4",]
    );
    // `c4` ran after `c3` (mutating boundary), so it observed the newly written file.
    let sent_tools = sent(&model.requests()[1], "tool");
    assert_eq!(sent_tools.len(), 4);
    assert!(sent_tools[0]["content"].as_str().unwrap().contains("alpha"));
    assert!(sent_tools[1]["content"].as_str().unwrap().contains("beta"));
    assert!(sent_tools[3]["content"].as_str().unwrap().contains("gamma"));
}
