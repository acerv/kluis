// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Line-based command-line coding agent.
//!
//! ```sh
//! harness-cli                      # interactive chat, Ctrl-D to quit
//! harness-cli "fix the typo in README.md"   # single prompt
//! ```
//!
//! The model comes from `HARNESS_MODEL` (default `deepseek-chat`); genai
//! picks the provider from its name and reads the matching API key, e.g.
//! `DEEPSEEK_API_KEY`.

use std::io::Write;

use harness_core::{Agent, Error, Event};
use tokio::io::{AsyncBufReadExt, BufReader};

const DEFAULT_MODEL: &str = "deepseek-chat";

/// Longest tool argument or result shown, in characters.
const SUMMARY_LEN: usize = 120;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    let mut agent = Agent::new(&model)?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        agent.prompt(&args.join(" "), print_event).await?;
        return Ok(());
    }

    println!("harness-cli ({model}), Ctrl-D to quit");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("> ");
        std::io::stdout().flush()?;

        let Some(line) = lines.next_line().await? else {
            println!();
            return Ok(());
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Keep the session alive on failures such as network errors.
        if let Err(err) = agent.prompt(line, print_event).await {
            eprintln!("error: {err}");
        }
    }
}

fn print_event(event: Event<'_>) {
    match event {
        Event::Text(text) => println!("{text}"),
        Event::ToolCall(call) => println!(
            "→ {} {}",
            call.fn_name,
            summary(&call.fn_arguments.to_string())
        ),
        Event::ToolResult(resp) => println!("← {}", summary(&resp.content)),
    }
}

/// First line of `text`, cut to [`SUMMARY_LEN`] characters.
fn summary(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(SUMMARY_LEN) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}
