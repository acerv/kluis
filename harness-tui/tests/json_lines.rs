// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Tests of the JSON lines `kl --output-format stream` prints.

use std::time::Duration;

use genai::chat::{ToolCall, ToolResponse, Usage};
use harness_core::Event;
use harness_tui::json_lines::event_json;
use serde_json::json;

#[test]
fn tool_calls_and_results_keep_their_id() {
    let call = ToolCall {
        call_id: "c1".into(),
        fn_name: "bash".into(),
        fn_arguments: json!({ "command": "ls" }),
        thought_signatures: None,
    };
    assert_eq!(
        event_json(Event::ToolCall(&call)),
        Some(json!({
            "type": "tool_call",
            "id": "c1",
            "name": "bash",
            "arguments": { "command": "ls" },
        }))
    );
    let resp = ToolResponse::new("c1", "README.md");
    assert_eq!(
        event_json(Event::ToolResult(&resp, None)),
        Some(json!({ "type": "tool_result", "id": "c1", "content": "README.md" }))
    );
}

#[test]
fn usage_matches_the_final_object() {
    let usage = Usage {
        prompt_tokens: Some(12),
        completion_tokens: Some(3),
        ..Default::default()
    };
    assert_eq!(
        event_json(Event::TurnEnd(Some(&usage))),
        Some(json!({ "type": "usage", "input": 12, "output": 3, "cached": 0 }))
    );
}

#[test]
fn usage_of_a_subagent_is_marked() {
    let usage = Usage {
        prompt_tokens: Some(7),
        completion_tokens: Some(2),
        ..Default::default()
    };
    assert_eq!(
        event_json(Event::TaskUsage(&usage)),
        Some(json!({
            "type": "usage",
            "input": 7,
            "output": 2,
            "cached": 0,
            "subagent": true,
        }))
    );
}

#[test]
fn retries_give_their_delay_in_milliseconds() {
    let retry = Event::Retry {
        attempt: 2,
        delay: Duration::from_secs(4),
        reason: "rate limited",
    };
    assert_eq!(
        event_json(retry),
        Some(json!({
            "type": "retry",
            "attempt": 2,
            "delay_ms": 4000,
            "reason": "rate limited",
        }))
    );
}

#[test]
fn pieces_of_text_are_left_out() {
    assert_eq!(event_json(Event::TextDelta("do")), None);
    assert_eq!(event_json(Event::TurnEnd(None)), None);
}
