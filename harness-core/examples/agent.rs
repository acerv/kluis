// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Minimal agent: a model reads and writes files through genai.
//!
//! ```sh
//! export DEEPSEEK_API_KEY=...   # or OPENAI_API_KEY, ANTHROPIC_API_KEY, ...
//! cargo run -p harness-core --example agent -- "Read Cargo.toml and summarize it in SUMMARY.txt"
//! ```
//!
//! Set `HARNESS_MODEL` to change model; genai picks the provider from it.

use harness_core::{Agent, Error, Event};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
    let prompt = std::env::args().nth(1).ok_or("usage: agent <prompt>")?;

    let mut agent = Agent::new(model)?;
    agent
        .prompt(&prompt, |event| match event {
            Event::Text(text) => println!("assistant: {text}"),
            Event::ToolCall(call) => println!("tool:      {} {}", call.fn_name, call.fn_arguments),
            Event::ToolResult(resp) => println!("result:    {}", resp.content),
        })
        .await?;
    Ok(())
}
