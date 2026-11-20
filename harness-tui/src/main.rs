// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Terminal coding agent with native sandboxing.
//!
//! ```sh
//! kl                           # interactive chat, Ctrl-D to quit
//! kl "fix the typo in README.md"   # single prompt
//! ```
//!
//! Uses an inline TUI when attached to a terminal and falls back to line mode
//! when stdin or stdout is not a terminal. genai picks the provider from the
//! model name and reads the matching API key, e.g. `DEEPSEEK_API_KEY`. The
//! harness runs in a hakoniwa container (see `sandbox.rs`) unless `--no-sandbox`
//! is given. Settings may also come from a config file (see `config.rs`).

mod config;
mod sandbox;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use genai::chat::ChatMessage;
use harness_core::tools::{run_worker, Executor, ToolContext};
use harness_core::{
    Agent, AuthConfig, Error, Event, SessionData, SessionStore, Thinking, TokenUsage,
};
use harness_tui::{summary, History, RunOptions, Session, Status};
use tokio::io::{AsyncBufReadExt, BufReader};

const DEFAULT_MODEL: &str = "deepseek-chat";

/// Terminal coding agent with native sandboxing.
#[derive(Parser)]
#[command(version)]
pub struct Args {
    /// Model to use; the provider is picked from its name [default: deepseek-chat].
    #[arg(long, env = "HARNESS_MODEL")]
    model: Option<String>,

    /// Thinking effort: default, low, medium, or high.
    #[arg(long, env = "HARNESS_THINKING", value_name = "LEVEL")]
    thinking: Option<Thinking>,

    /// Continue the most recent session for the current folder.
    #[arg(short = 'c', long = "continue", conflicts_with = "no_session")]
    continue_session: bool,

    /// Resume a previous session (opens a picker when no ID is given).
    #[arg(
        short = 'r',
        long = "resume",
        value_name = "ID",
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with_all = ["continue_session", "no_session"]
    )]
    resume_session: Option<String>,

    /// Do not read or write session files.
    #[arg(long)]
    no_session: bool,

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

    /// Hide this path from the harness and its commands (repeatable).
    #[arg(long, value_name = "PATH")]
    deny_read: Vec<PathBuf>,

    /// Also let the harness connect to this TCP port, e.g. for a local
    /// model (repeatable).
    #[arg(long, value_name = "PORT")]
    allow_connect: Vec<u16>,

    /// Allow any network, to commands too.
    #[arg(long)]
    network: bool,

    /// Processes and threads the harness and its commands may run at once
    /// [default: 1024].
    #[arg(long, value_name = "NUM", value_parser = clap::value_parser!(u64).range(1..))]
    max_processes: Option<u64>,

    /// Size of the private /tmp in bytes, or with a K, M or G suffix.
    #[arg(long, value_name = "SIZE", value_parser = config::parse_size)]
    tmp_size: Option<u64>,

    /// Virtual memory limit for commands in bytes, or with a K, M or G suffix.
    #[arg(long, value_name = "SIZE", value_parser = config::parse_size)]
    max_memory: Option<u64>,

    /// Also pass this environment variable to commands (repeatable).
    #[arg(long, value_name = "NAME")]
    pass_env: Vec<String>,

    /// Config file [default: ~/.kluis/config.toml]
    #[arg(long, env = config::ENV, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Internal mode: run the NDJSON tool worker inside the sandbox container.
    #[arg(long, hide = true)]
    tool_executor: bool,

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
    if args.tool_executor {
        let cwd = std::env::current_dir()?;
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_worker(cwd, args.max_memory))?;
        return Ok(ExitCode::SUCCESS);
    }

    let config_model = apply_config(&mut args)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args, config_model))?;
    Ok(ExitCode::SUCCESS)
}

/// Add the settings of the config file to `args`, leave in `args.config` the
/// path of the file used (if any), and return the config's default model so
/// resumed sessions keep their own model unless `--model` was explicitly set.
fn apply_config(args: &mut Args) -> Result<Option<String>, Error> {
    let (path, required) = match args.config.take() {
        Some(path) => (std::path::absolute(path)?, true),
        None => match config::default_path() {
            Some(path) => (path, false),
            None => return Ok(None),
        },
    };
    let config = config::load(&path, required)?;
    args.thinking = args.thinking.or(config.thinking);
    args.allow_read.extend(config.sandbox.read);
    args.allow_write.extend(config.sandbox.write);
    args.deny_read.extend(config.sandbox.deny);
    args.allow_connect.extend(config.sandbox.connect);
    args.network |= config.sandbox.network;
    args.max_processes = args.max_processes.or(config.sandbox.max_processes);
    args.tmp_size = args.tmp_size.or(config.sandbox.tmp_size);
    args.max_memory = args.max_memory.or(config.sandbox.max_memory);
    args.pass_env.extend(config.sandbox.env);
    args.config = Some(path);
    Ok(config.model)
}

async fn run(args: Args, config_model: Option<String>) -> Result<(), Error> {
    let cwd = std::env::current_dir()?;
    let auth = match config::auth_path() {
        Some(path) => AuthConfig::load(&path)?,
        None => AuthConfig::default(),
    };
    let client = auth.client()?;
    let system = harness_core::system_prompt(
        &cwd,
        &harness_core::find_agents_files(&cwd, config::agents_path().as_deref()),
    );
    let store = (!args.no_session)
        .then(config::sessions_dir)
        .flatten()
        .map(|root| SessionStore::new(root, &cwd));
    let default_model = args
        .model
        .as_deref()
        .or(config_model.as_deref())
        .unwrap_or(DEFAULT_MODEL);

    let mut picker_entries = Vec::new();
    let (model, history, mut session) = if args.continue_session {
        let store = store.ok_or("cannot continue session: HOME is not set")?;
        let Some(data) = store.latest()? else {
            return Err(format!("no session to continue for {}", cwd.display()).into());
        };
        open_loaded_session(store, data, args.model.as_deref())?
    } else if let Some(ref query) = args.resume_session {
        let store = store.ok_or("cannot resume session: HOME is not set")?;
        if query.is_empty() {
            if !args.prompt.is_empty() || !io::stdin().is_terminal() || !io::stdout().is_terminal()
            {
                return Err(
                    "--resume without an ID requires an interactive terminal (pass --resume <id> or --continue)"
                        .into(),
                );
            }
            let entries = store.list()?;
            if entries.is_empty() {
                return Err(format!("no sessions to resume for {}", cwd.display()).into());
            }
            picker_entries = entries;
            let model = default_model.to_owned();
            let writer = store.create(&model);
            (
                model,
                Vec::new(),
                Some(Session::new(store, writer, TokenUsage::default())),
            )
        } else {
            let data = store.load(query)?;
            open_loaded_session(store, data, args.model.as_deref())?
        }
    } else {
        let model = default_model.to_owned();
        let session = store.map(|store| {
            let writer = store.create(&model);
            Session::new(store, writer, TokenUsage::default())
        });
        (model, Vec::new(), session)
    };

    let executor = if args.no_sandbox {
        // Even without the container, keep local bash commands away from API
        // keys in our memory and environment.
        harness_sandbox::protect_self()?;
        let mut tools = ToolContext::default();
        tools.bash.env = Some(harness_sandbox::command_env(
            std::env::vars_os(),
            &args.pass_env,
        ));
        Executor::Local(tools)
    } else {
        sandbox::executor(&args, &cwd)?
    };

    let mut agent = Agent::with_client(client, &model)
        .with_thinking(args.thinking.unwrap_or_default())
        .with_system(system)
        .with_history(history)
        .with_executor(executor);

    if !args.prompt.is_empty() {
        prompt_and_persist(&mut agent, session.as_mut(), &args.prompt.join(" ")).await?;
        return Ok(());
    }

    let sandbox = sandbox_label(&args);
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        let status = Status {
            sandbox,
            folder: current_folder(),
        };
        let history = config::history_path().map(History::load);
        return harness_tui::run(
            agent,
            status,
            RunOptions {
                session,
                history,
                picker_entries,
            },
        )
        .await;
    }

    println!(
        "kluis ({} ({}), {sandbox}), Ctrl-D to quit",
        agent.model(),
        agent.thinking()
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("> ");
        io::stdout().flush()?;

        let Some(line) = lines.next_line().await? else {
            println!();
            return Ok(());
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Keep the session alive on failures such as network errors.
        if let Err(err) = prompt_and_persist(&mut agent, session.as_mut(), line).await {
            eprintln!("error: {err}");
        }
    }
}

fn open_loaded_session(
    store: SessionStore,
    data: SessionData,
    model_override: Option<&str>,
) -> Result<(String, Vec<ChatMessage>, Option<Session>), Error> {
    let model = model_override.unwrap_or(&data.meta.model).to_owned();
    let mut writer = store.open(&data);
    if model_override.is_some() && model != data.meta.model {
        writer.append_model(&model)?;
    }
    let session = Session::new(store, writer, data.usage);
    Ok((model, data.messages, Some(session)))
}

async fn prompt_and_persist(
    agent: &mut Agent,
    session: Option<&mut Session>,
    text: &str,
) -> Result<(), Error> {
    let prev_len = agent.history().len();
    let mut usage = TokenUsage::default();
    let res = agent
        .prompt(text, |event| {
            if let Event::TurnEnd(Some(u)) = event {
                usage += TokenUsage::from_genai(u);
            }
            print_event(event);
        })
        .await;
    if let Some(session) = session {
        let added = &agent.history()[prev_len..];
        if let Err(err) = session.record_prompt(added, usage) {
            eprintln!("warning: could not save session: {err}");
        }
    }
    res.map(|_| ())
}

fn sandbox_label(args: &Args) -> String {
    let base = if args.no_sandbox {
        "no sandbox"
    } else if args.strict {
        "strict sandbox"
    } else {
        "sandboxed"
    };
    // Commands can reach the network: worth knowing.
    if args.network && !args.no_sandbox {
        format!("{base}, network")
    } else {
        base.to_owned()
    }
}

fn current_folder() -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return ".".to_owned();
    };
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        if let Ok(rel) = cwd.strip_prefix(&home) {
            return if rel.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", rel.display())
            };
        }
    }
    cwd.display().to_string()
}

fn print_event(event: Event<'_>) {
    match event {
        Event::TextDelta(text) => {
            print!("{text}");
            // Show the text as it comes, not line by line.
            let _ = io::stdout().flush();
        }
        // Already printed piece by piece: end its line.
        Event::Text(_) => println!(),
        Event::TurnStart | Event::ReasoningDelta(_) | Event::ToolOutput(_) | Event::TurnEnd(_) => {}
        Event::ToolCall(call) => println!(
            "→ {} {}",
            call.fn_name,
            summary(&call.fn_arguments.to_string())
        ),
        Event::ToolResult(resp, _) => println!("← {}", summary(&resp.content)),
    }
}
