// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Prompt events as newline-delimited JSON, for `kl --output-format stream`.
//!
//! Every line is an object whose `type` names the event. Text comes once
//! per turn, not piece by piece, and the last line is the `result` or the
//! `error` that `--output-format json` prints.

use genai::chat::Usage;
use harness_core::Event;
use harness_core::TokenUsage;
use serde_json::{json, Value};

/// Print `event` as one JSON line, when scripts have a use for it.
pub fn print_event(event: Event<'_>) {
    if let Some(line) = event_json(event) {
        // Line buffered: each event shows as soon as it happens.
        println!("{line}");
    }
}

pub fn usage_json(usage: TokenUsage) -> Value {
    json!({
        "input": usage.input,
        "output": usage.output,
        "cached": usage.cached,
    })
}

/// The `usage` line of a turn.
fn turn_usage(usage: &Usage) -> Value {
    let mut value = usage_json(TokenUsage::from_genai(usage));
    value["type"] = "usage".into();
    value
}

fn event_json(event: Event<'_>) -> Option<Value> {
    let value = match event {
        Event::Text(text) => json!({ "type": "text", "text": text }),
        Event::ToolCall(call) => json!({
            "type": "tool_call",
            "id": call.call_id,
            "name": call.fn_name,
            "arguments": call.fn_arguments,
        }),
        Event::ToolResult(resp, _) => json!({
            "type": "tool_result",
            "id": resp.call_id,
            "content": resp.content,
        }),
        Event::TurnEnd(Some(usage)) => turn_usage(usage),
        Event::TaskUsage(usage) => {
            let mut value = turn_usage(usage);
            value["subagent"] = true.into();
            value
        }
        Event::Retry {
            attempt,
            delay,
            reason,
        } => json!({
            "type": "retry",
            "attempt": attempt,
            "delay_ms": delay.as_millis() as u64,
            "reason": reason,
        }),
        Event::Truncated => json!({ "type": "truncated" }),
        Event::Compacting { context_tokens } => {
            json!({ "type": "compacting", "context_tokens": context_tokens })
        }
        Event::Compacted { before, .. } => json!({ "type": "compacted", "messages": before }),
        Event::CompactFailed(err) => json!({ "type": "compact_failed", "error": err }),
        Event::Pruned(count) => json!({ "type": "pruned", "tool_results": count }),
        Event::Messages(_)
        | Event::Steered(_)
        | Event::TurnStart
        | Event::ReasoningDelta(_)
        | Event::TextDelta(_)
        | Event::ToolOutput(_)
        | Event::TurnEnd(None) => return None,
    };
    Some(value)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use genai::chat::{ToolCall, ToolResponse};

    use super::*;

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
}
