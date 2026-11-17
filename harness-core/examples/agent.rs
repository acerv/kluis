// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Minimal agent: a model reads and writes files through genai.
//!
//! ```sh
//! export DEEPSEEK_API_KEY=...   # or OPENAI_API_KEY, ANTHROPIC_API_KEY, ...
//! cargo run -p harness-core --example agent -- "Read Cargo.toml and summarize it in SUMMARY.txt"
//! ```
//!
//! Set `HARNESS_MODEL` to change model; genai picks the provider from it.

use harness_core::{Agent, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
    let prompt = std::env::args().nth(1).ok_or("usage: agent <prompt>")?;

    let mut agent = Agent::new(model)?;
    println!("{}", agent.prompt(&prompt).await?);
    Ok(())
}
