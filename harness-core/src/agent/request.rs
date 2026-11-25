// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Asking the model for its next turn.

use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatRole, ChatStreamEvent, ContentPart,
    MessageContent, StopReason, Usage,
};
use genai::ModelIden;
use serde_json::json;
use uuid::Uuid;

use super::Agent;
use crate::retry::EndedEarly;
use crate::{Error, Event};

impl Agent {
    /// Text the login used for `kind` requires as the first system prompt
    /// block.
    fn system_prefix(&self, kind: AdapterKind) -> Option<&'static str> {
        self.oauth.as_ref()?.system_prefix(kind)
    }

    /// Like [`next_turn`](Self::next_turn), retrying failures that may pass
    /// as the [`RetryPolicy`] says.
    pub(super) async fn next_turn_retrying(
        &self,
        extra_user: Option<&str>,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> Result<Turn, Error> {
        let mut attempt = 0;
        loop {
            let err = match self.next_turn(extra_user, on_event).await {
                Ok(turn) => return Ok(turn),
                Err(err) => err,
            };
            attempt += 1;
            let Some((delay, reason)) = self.retry.delay(attempt, err.as_ref()) else {
                return Err(err);
            };
            on_event(Event::Retry {
                attempt,
                delay,
                reason: &reason,
            });
            tokio::time::sleep(delay).await;
        }
    }

    /// Build the per-turn request for `model`, applying any OAuth system
    /// prefix, an optional transient trailing user message, and Anthropic
    /// prompt-cache breakpoints without mutating `self.req` or shifting the
    /// cloned message buffer.
    fn build_request(
        &self,
        model: &ModelIden,
        prefix: Option<&'static str>,
        extra_user: Option<&str>,
    ) -> ChatRequest {
        let anthropic_cache = uses_anthropic_cache(model);
        let (system, leading_system) = match (prefix, self.req.system.as_deref()) {
            (Some(prefix), Some(sys)) => (Some(prefix.to_owned()), Some(ChatMessage::system(sys))),
            (Some(prefix), None) if anthropic_cache => (None, Some(ChatMessage::system(prefix))),
            (Some(prefix), None) => (Some(prefix.to_owned()), None),
            (None, Some(sys)) if anthropic_cache => (None, Some(ChatMessage::system(sys))),
            (None, sys) => (sys.map(str::to_owned), None),
        };
        let has_leading_system = leading_system.is_some();
        let leading_system = leading_system.map(|msg| {
            if anthropic_cache {
                msg.with_options(CacheControl::Ephemeral)
            } else {
                msg
            }
        });

        let mut messages = Vec::with_capacity(
            self.req.messages.len()
                + usize::from(has_leading_system)
                + usize::from(extra_user.is_some()),
        );
        if let Some(msg) = leading_system {
            messages.push(msg);
        }
        messages.extend(self.req.messages.iter().cloned());

        let mut tools = self.req.tools.clone();
        if let Some(tools) = tools.as_mut().filter(|_| !self.permissions.is_empty()) {
            tools.retain(|tool| self.permissions.offers(tool.name.as_ref()));
        }
        if anthropic_cache {
            if !has_leading_system {
                if let Some(last_tool) = tools.as_mut().and_then(|t| t.last_mut()) {
                    last_tool.cache_control = Some(CacheControl::Ephemeral);
                }
            }
            // Mark only persisted history so transient prompts (e.g. `/compact`)
            // hit the existing prefix cache without writing a discarded tail.
            mark_conversation_cache(&mut messages);
        }
        if let Some(text) = extra_user {
            messages.push(ChatMessage::user(text));
        }

        ChatRequest {
            system,
            messages,
            tools,
            ..ChatRequest::default()
        }
    }

    /// Ask the model for its next turn, reporting its text as it streams,
    /// and return it with its token usage.
    ///
    /// The turn joins the history only once complete: a cut stream, like
    /// pi does, is never sent back to the model.
    async fn next_turn(
        &self,
        extra_user: Option<&str>,
        on_event: &mut impl FnMut(Event<'_>),
    ) -> Result<Turn, Error> {
        let target = self.client.resolve_service_target(&self.model).await?;
        let mut options = ChatOptions::default()
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_reasoning_content(true)
            .with_capture_usage(true);
        if let Some(effort) = self.thinking.reasoning_effort() {
            options = options.with_reasoning_effort(effort);
        }
        if uses_openai_cache_key(target.model.adapter_kind) {
            let mut buf = Uuid::encode_buffer();
            options =
                options.with_prompt_cache_key(&*self.cache_key.hyphenated().encode_lower(&mut buf));
        }
        let prefix = self.system_prefix(target.model.adapter_kind);
        let req = self.build_request(&target.model, prefix, extra_user);
        let res = self
            .client
            .exec_chat_stream(target, req, Some(&options))
            .await?;
        let reasoning_back = needs_reasoning_back(res.model_iden.adapter_kind);
        let mut stream = res.stream;
        while let Some(event) = stream.next().await {
            match event? {
                ChatStreamEvent::Chunk(chunk) => on_event(Event::TextDelta(&chunk.content)),
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    on_event(Event::ReasoningDelta(&chunk.content))
                }
                ChatStreamEvent::End(end) => {
                    let truncated = end
                        .captured_stop_reason
                        .as_ref()
                        .is_some_and(StopReason::is_max_tokens);
                    let mut parts = end.captured_content.unwrap_or_default().into_parts();
                    // Providers refuse blank text and arguments that are
                    // not an object when the turn is sent back.
                    parts.retain(
                        |part| !matches!(part, ContentPart::Text(t) if t.trim().is_empty()),
                    );
                    let mut invalid = Vec::new();
                    for part in &mut parts {
                        if let ContentPart::ToolCall(call) = part {
                            let args = &mut call.fn_arguments;
                            if !args.is_object() {
                                // No arguments at all is a call without any.
                                let none = args.is_null()
                                    || args.as_str().is_some_and(|s| s.trim().is_empty());
                                if !none {
                                    invalid.push(call.call_id.clone());
                                }
                                *args = json!({});
                            }
                        }
                    }
                    let mut content = MessageContent::from_parts(parts);
                    // Unless the turn already carries its reasoning, e.g.
                    // signed, which an extra part would break.
                    let carried = content
                        .parts()
                        .iter()
                        .any(|p| p.is_reasoning_content() || p.is_thought_signature());
                    if let Some(reasoning) = end.captured_reasoning_content {
                        if reasoning_back && !carried && !reasoning.is_empty() {
                            content.prepend(ContentPart::ReasoningContent(reasoning));
                        }
                    }
                    return Ok(Turn {
                        content,
                        usage: end.captured_usage,
                        truncated,
                        invalid,
                    });
                }
                _ => {}
            }
        }
        Err(EndedEarly.into())
    }
}

pub(super) struct Turn {
    pub(super) content: MessageContent,
    pub(super) usage: Option<Usage>,
    pub(super) truncated: bool,
    /// Ids of the tool calls whose arguments were not a JSON object.
    pub(super) invalid: Vec<String>,
}

/// Whether the provider wants the reasoning of a turn back in the next
/// requests, as DeepSeek and Kimi do for tool calls in thinking mode.
/// Others may refuse it, so it stays out.
fn needs_reasoning_back(kind: AdapterKind) -> bool {
    matches!(
        kind,
        AdapterKind::DeepSeek | AdapterKind::Kimi | AdapterKind::Moonshot
    )
}

/// Whether `model` uses Anthropic's block-level `cache_control` protocol.
fn uses_anthropic_cache(model: &ModelIden) -> bool {
    let (_, name) = model.model_name.namespace_and_name();
    match model.adapter_kind {
        AdapterKind::Anthropic => true,
        AdapterKind::Vertex => name.starts_with("claude"),
        _ => false,
    }
}

/// Whether `kind` accepts OpenAI's `prompt_cache_key` routing hint.
fn uses_openai_cache_key(kind: AdapterKind) -> bool {
    matches!(kind, AdapterKind::OpenAI | AdapterKind::OpenAIResp)
}

/// Place rolling Anthropic prompt-cache breakpoints on `messages`:
///
/// 1. Last cacheable user/tool message so the request writes the current
///    prefix into the cache for the next turn.
/// 2. Last cacheable user/tool message before the final assistant turn,
///    matching the previous turn's write breakpoint even when the latest
///    turn added more than 20 content blocks.
fn mark_conversation_cache(messages: &mut [ChatMessage]) {
    mark_last_cacheable(messages);
    if let Some(last_assistant) = messages.iter().rposition(|m| m.role == ChatRole::Assistant) {
        mark_last_cacheable(&mut messages[..last_assistant]);
    }
}

fn mark_last_cacheable(messages: &mut [ChatMessage]) {
    if let Some(msg) = messages.iter_mut().rfind(|m| is_cacheable_message(m)) {
        msg.options
            .get_or_insert_with(Default::default)
            .cache_control = Some(CacheControl::Ephemeral);
    }
}

/// Whether `msg` is a user or tool turn with non-empty content that
/// Anthropic accepts a `cache_control` breakpoint on.
fn is_cacheable_message(msg: &ChatMessage) -> bool {
    match msg.role {
        ChatRole::User => msg.content.parts().iter().any(|part| match part {
            ContentPart::Text(text) => !text.is_empty(),
            ContentPart::Binary(_) | ContentPart::ToolResponse(_) | ContentPart::Custom(_) => true,
            _ => false,
        }),
        ChatRole::Tool => !msg.content.tool_responses().is_empty(),
        ChatRole::System | ChatRole::Assistant => false,
    }
}
