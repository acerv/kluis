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
        crate::outln!("{line}");
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

/// The line for `event`, or `None` for the events scripts have no use for.
pub fn event_json(event: Event<'_>) -> Option<Value> {
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
