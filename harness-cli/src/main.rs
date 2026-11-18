// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Line-based command-line coding agent.
//!
//! ```sh
//! kl-cli                           # interactive chat, Ctrl-D to quit
//! kl-cli "fix the typo in README.md"   # single prompt
//! ```
//!
//! genai picks the provider from the model name and reads the matching API
//! key, e.g. `DEEPSEEK_API_KEY`. The harness runs in a hakoniwa container
//! (see `sandbox.rs`) unless `--no-sandbox` is given. Settings may also come
//! from a config file (see `config.rs`).

mod config;
mod sandbox;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use harness_core::tools::ToolContext;
use harness_core::{Agent, Error, Event};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Longest tool argument or result shown, in characters.
const SUMMARY_LEN: usize = 120;

/// Line-based command-line coding agent.
#[derive(Parser)]
#[command(version)]
pub struct Args {
    /// Model to use; the provider is picked from its name.
    #[arg(long, env = "HARNESS_MODEL", default_value = "deepseek-chat")]
    model: String,

    /// Run without the sandbox.
    #[arg(long)]
    no_sandbox: bool,

    /// Strict sandbox: make the current folder read-only by default.
    #[arg(long, conflicts_with = "no_sandbox")]
    strict: bool,

    /// Also allow reading this path (repeatable).
    #[arg(long, value_name = "PATH")]
    allow_read: Vec<PathBuf>,

    /// Also allow changing this path (repeatable).
    #[arg(long, value_name = "PATH")]
    allow_write: Vec<PathBuf>,

    /// Also let the harness connect to this TCP port, e.g. for a local
    /// model (repeatable).
    #[arg(long, value_name = "PORT")]
    allow_connect: Vec<u16>,

    /// Allow any network, to commands too.
    #[arg(long)]
    network: bool,

    /// Also pass this environment variable to commands (repeatable).
    #[arg(long, value_name = "NAME")]
    pass_env: Vec<String>,

    /// Config file [default: ~/.config/harness/config.toml]
    #[arg(long, env = config::ENV, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Prompt to run once; starts an interactive chat when absent.
    prompt: Vec<String>,
}

fn main() -> ExitCode {
    match launch(Args::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn launch(mut args: Args) -> Result<ExitCode, Error> {
    apply_config(&mut args)?;
    if args.no_sandbox {
        start(args, ToolContext::default())?;
    } else if sandbox::inside() {
        let tools = sandbox::tools(&args);
        start(args, tools)?;
    } else {
        return sandbox::run(&args);
    }
    Ok(ExitCode::SUCCESS)
}

/// Add the settings of the config file to `args`, and leave in
/// `args.config` the absolute path of the file used, if any.
fn apply_config(args: &mut Args) -> Result<(), Error> {
    let (path, required) = match args.config.take() {
        Some(path) => (std::path::absolute(path)?, true),
        None => match config::default_path() {
            Some(path) => (path, false),
            None => return Ok(()),
        },
    };
    let Some(config) = config::load(&path, required)? else {
        return Ok(());
    };
    args.allow_read.extend(config.sandbox.read);
    args.allow_write.extend(config.sandbox.write);
    args.allow_connect.extend(config.sandbox.connect);
    args.network |= config.sandbox.network;
    args.pass_env.extend(config.sandbox.env);
    args.config = Some(path);
    Ok(())
}

fn start(args: Args, mut tools: ToolContext) -> Result<(), Error> {
    // Commands run as the same user: keep them away from the API keys in
    // our memory and environment.
    harness_sandbox::protect_self()?;
    tools.bash.env = Some(harness_sandbox::command_env(
        std::env::vars_os(),
        &args.pass_env,
    ));

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args, tools))
}

async fn run(args: Args, tools: ToolContext) -> Result<(), Error> {
    let mut agent = Agent::new(&args.model)?.with_tool_context(tools);

    if !args.prompt.is_empty() {
        agent.prompt(&args.prompt.join(" "), print_event).await?;
        return Ok(());
    }

    let sandbox = if args.no_sandbox {
        "no sandbox"
    } else if args.strict {
        "strict sandbox"
    } else {
        "sandboxed"
    };
    // Commands can reach the network: worth knowing.
    let network = if args.network && !args.no_sandbox {
        ", network"
    } else {
        ""
    };
    println!("kluis ({}, {sandbox}{network}), Ctrl-D to quit", args.model);
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
        Event::TextDelta(text) => {
            print!("{text}");
            // Show the text as it comes, not line by line.
            let _ = std::io::stdout().flush();
        }
        // Already printed piece by piece: end its line.
        Event::Text(_) => println!(),
        Event::TurnStart | Event::ReasoningDelta(_) | Event::TurnEnd(_) => {}
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
