//! Minimal agent loop: a model reads and writes files through genai.
//!
//! ```sh
//! export DEEPSEEK_API_KEY=...   # or OPENAI_API_KEY, ANTHROPIC_API_KEY, ...
//! cargo run --example agent -- "Read Cargo.toml and summarize it in SUMMARY.txt"
//! ```
//!
//! Set `HARNESS_MODEL` to change model; genai picks the provider from it.

use genai::chat::{ChatMessage, ChatRequest, ToolCall, ToolResponse};
use genai::Client;
use harness_tools::{ReadTool, WriteTool};
use serde_json::{json, Value};

const MAX_TURNS: usize = 10;

type Error = Box<dyn std::error::Error>;

/// Run one tool call and return its result as JSON text for the model.
async fn run(call: &ToolCall) -> String {
    let args = call.fn_arguments.clone();

    let result: Result<Value, Error> = match call.fn_name.as_str() {
        "read" => match serde_json::from_value::<ReadTool>(args) {
            Ok(tool) => tool.run().await.map(|out| json!(out)).map_err(Into::into),
            Err(e) => Err(e.into()),
        },
        "write" => match serde_json::from_value::<WriteTool>(args) {
            Ok(tool) => tool.run().await.map(|out| json!(out)).map_err(Into::into),
            Err(e) => Err(e.into()),
        },
        other => Err(format!("unknown tool {other}").into()),
    };

    match result {
        Ok(output) => output.to_string(),
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
    let prompt = std::env::args().nth(1).ok_or("usage: agent <prompt>")?;

    let client = Client::new()?;
    let mut req = ChatRequest::new(vec![ChatMessage::user(prompt)])
        .with_tools([ReadTool::definition(), WriteTool::definition()]);

    for _ in 0..MAX_TURNS {
        let res = client.exec_chat(&model, req.clone(), None).await?;
        if let Some(text) = res.first_text() {
            println!("assistant: {text}");
        }

        let calls: Vec<ToolCall> = res.tool_calls().into_iter().cloned().collect();
        if calls.is_empty() {
            return Ok(());
        }

        // The assistant turn must precede the tool results in the history.
        req = req.append_message(ChatMessage::assistant(res.content));
        for call in &calls {
            println!("tool:      {} {}", call.fn_name, call.fn_arguments);
            let content = run(call).await;
            println!("result:    {content}");
            req = req.append_message(ToolResponse::from_tool_call(call, content));
        }
    }

    Err(format!("stopped after {MAX_TURNS} turns").into())
}
