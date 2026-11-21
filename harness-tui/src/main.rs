// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Terminal coding agent with native sandboxing.
//!
//! ```sh
//! kl                           # interactive chat, Ctrl-D to quit
//! kl "fix the typo in README.md"   # single prompt
//! kl --login anthropic         # use a Claude Pro/Max subscription
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
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use genai::adapter::AdapterKind;
use genai::chat::ChatMessage;
use harness_core::oauth::flow::Login;
use harness_core::oauth::{OAuthProvider, OAuthStore};
use harness_core::tools::{run_worker, Executor, ToolContext};
use harness_core::{
    provider_name, Agent, AuthConfig, CredentialSource, Error, Event, SessionData, SessionStore,
    Thinking, TokenUsage,
};
use harness_sandbox::{GitGuard, NetworkAccess};
use harness_tui::{
    git_notices, retry_notice, summary, BannerLine, History, RunOptions, Session, Status,
};
use tokio::io::{AsyncBufReadExt, BufReader};

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

    /// Log in to a provider subscription (anthropic), then exit.
    #[arg(long, value_name = "PROVIDER", conflicts_with_all = ["logout", "prompt"])]
    login: Option<OAuthProvider>,

    /// Forget the login to a provider, then exit.
    #[arg(long, value_name = "PROVIDER", conflicts_with = "prompt")]
    logout: Option<OAuthProvider>,

    /// Internal mode: run the NDJSON tool worker inside the sandbox container.
    #[arg(long, hide = true)]
    tool_executor: bool,

    /// Internal: the network the tool worker's commands get (none, ports or
    /// any), which decides the sockets they may create.
    #[arg(long, hide = true, requires = "tool_executor", default_value = "none")]
    network_access: NetworkAccess,

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
    let mut below_init = false;
    if args.tool_executor {
        // Commands run as the same user: keep them from reading the
        // worker's environment or writing forged results into its pipes.
        harness_sandbox::protect_self()?;
        // Before the runtime starts its threads, though the filter covers
        // them anyway.
        harness_sandbox::restrict_syscalls(args.network_access)?;
        // An init reaps what commands leave behind, and lets the worker
        // stop it between prompts.
        below_init = harness_sandbox::start_init()?;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if args.tool_executor {
        let cwd = std::env::current_dir()?;
        runtime.block_on(run_worker(cwd, args.max_memory, below_init))?;
    } else if let Some(provider) = args.login {
        runtime.block_on(login(provider))?;
    } else if let Some(provider) = args.logout {
        runtime.block_on(logout(provider))?;
    } else {
        let config_model = apply_config(&mut args)?;
        runtime.block_on(run(args, config_model))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn oauth_store() -> Result<OAuthStore, Error> {
    let path = config::oauth_path().ok_or("cannot store logins: HOME is not set")?;
    OAuthStore::load(path)
}

/// Log in to `provider` and save the login.
async fn login(provider: OAuthProvider) -> Result<(), Error> {
    let store = oauth_store()?;
    let token = match store.start_login(provider).await? {
        Login::Paste(login) => {
            println!(
                "Open this page to log in to {provider}:\n\n{}\n",
                login.url()
            );
            print!("Paste the authorization code: ");
            io::stdout().flush()?;
            let mut input = String::new();
            BufReader::new(tokio::io::stdin())
                .read_line(&mut input)
                .await?;
            if input.trim().is_empty() {
                return Err("login cancelled".into());
            }
            login.finish(&input).await?
        }
        Login::Callback(login) => {
            println!(
                "Open this page to log in to {provider}:\n\n{}\n",
                login.url()
            );
            println!("Waiting for the browser...");
            login.finish().await?
        }
        Login::Device(login) => {
            println!(
                "Open {} and enter the code {} to log in to {provider}.",
                login.url(),
                login.user_code()
            );
            println!("Waiting for approval...");
            login.finish().await?
        }
    };
    store.finish_login(provider, &token).await?;
    println!("Logged in to {provider}.");
    if let Some(var) = provider
        .adapter()
        .default_key_env_name()
        .filter(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()))
    {
        eprintln!("warning: {var} is set and is used instead of the login");
    }
    Ok(())
}

async fn logout(provider: OAuthProvider) -> Result<(), Error> {
    if oauth_store()?.logout(provider).await? {
        println!("Logged out of {provider}.");
    } else {
        println!("Not logged in to {provider}.");
    }
    Ok(())
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
    args.thinking = args.thinking.or(config.generic.thinking);
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
    Ok(config.generic.model)
}

async fn run(args: Args, config_model: Option<String>) -> Result<(), Error> {
    let cwd = std::env::current_dir()?;
    let mut auth = match config::auth_path() {
        Some(path) => AuthConfig::load(&path)?,
        None => AuthConfig::default(),
    };
    if let Some(path) = config::oauth_path() {
        auth = auth.with_oauth(Arc::new(OAuthStore::load(path)?));
    }
    let client = auth.client()?;
    let adapters = auth.configured_adapters();
    let oauth = auth.oauth_logins();
    let agents = harness_core::find_agents_files(&cwd, config::agents_path().as_deref());
    let system = harness_core::system_prompt(&cwd, &agents);
    let store = (!args.no_session)
        .then(config::sessions_dir)
        .flatten()
        .map(|root| SessionStore::new(root, &cwd));
    let default_model = args
        .model
        .as_deref()
        .or(config_model.as_deref())
        .unwrap_or(config::DEFAULT_MODEL);

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
    let credential = AdapterKind::from_model(&model)
        .ok()
        .map(|kind| (kind, auth.credential_source(kind)));
    drop(auth);

    let (executor, mut git_guard) = if args.no_sandbox {
        // Even without the container, keep local bash commands away from API
        // keys in our memory and environment.
        harness_sandbox::protect_self()?;
        let mut tools = ToolContext::default();
        tools.bash.env = Some(harness_sandbox::command_env(
            std::env::vars_os(),
            &args.pass_env,
        ));
        (Executor::Local(tools), None)
    } else {
        let (executor, guard) = sandbox::executor(&args, &cwd)?;
        (executor, Some(guard))
    };

    let mut agent = Agent::with_client(client, &model)
        .with_oauth(oauth)
        .with_thinking(args.thinking.unwrap_or_default())
        .with_system(system)
        .with_history(history)
        .with_executor(executor);
    // Fetching runs on the host: only when the model may reach the network
    // anyway, which is always the case without the sandbox.
    if args.network || args.no_sandbox {
        agent = agent.with_web_fetch()?;
    }

    // Commands can write a repository only in a writable folder.
    let writable = !args.no_sandbox && (!args.strict || !args.allow_write.is_empty());
    let git_warning = writable.then(sandbox::bare_repository_warning).flatten();
    let full_screen =
        args.prompt.is_empty() && io::stdin().is_terminal() && io::stdout().is_terminal();
    if let Some(warning) = git_warning.as_ref().filter(|_| !full_screen) {
        eprintln!("warning: {warning}");
    }

    if !args.prompt.is_empty() {
        let text = args.prompt.join(" ");
        prompt_and_persist(&mut agent, session.as_mut(), git_guard.as_mut(), &text).await?;
        return Ok(());
    }

    let sandbox = sandbox_label(&args);
    if full_screen {
        let status = Status {
            sandbox,
            folder: current_folder(),
        };
        let mut banner = vec![
            BannerLine::new(format!("kluis v{}", env!("CARGO_PKG_VERSION"))),
            auth_line(&model, credential),
            BannerLine::new(sandbox_details(&args, &cwd)),
            BannerLine::new(context_line(&agents)),
            BannerLine::new(session_line(session.as_ref(), agent.history().len())),
        ];
        banner.extend(git_warning.map(BannerLine::warning));
        let history = config::history_path().map(History::load);
        return harness_tui::run(
            agent,
            status,
            RunOptions {
                session,
                history,
                picker_entries,
                adapters,
                banner,
                git_guard,
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
        let res = prompt_and_persist(&mut agent, session.as_mut(), git_guard.as_mut(), line).await;
        if let Err(err) = res {
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
    git_guard: Option<&mut GitGuard>,
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
    // Before the git check: a program left running could recreate a
    // repository right after it.
    agent.stop_background().await;
    let added = &agent.history()[prev_len..];
    if let Some(session) = session {
        if let Err(err) = session.record_prompt(added, usage) {
            eprintln!("warning: could not save session: {err}");
        }
    }
    for notice in git_notices(git_guard, added) {
        eprintln!("{notice}");
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

/// What the sandbox lets the harness change and reach, for the banner.
fn sandbox_details(args: &Args, cwd: &Path) -> String {
    if args.no_sandbox {
        return "no sandbox".to_owned();
    }
    let write: Vec<_> = (!args.strict)
        .then_some(cwd)
        .into_iter()
        .chain(args.allow_write.iter().map(PathBuf::as_path))
        .map(tilde)
        .collect();
    let access = if write.is_empty() {
        "read-only".to_owned()
    } else {
        format!("write {}", write.join(", "))
    };
    let network = if args.network {
        "any network".to_owned()
    } else if args.allow_connect.is_empty() {
        "no network".to_owned()
    } else {
        let ports: Vec<_> = args.allow_connect.iter().map(u16::to_string).collect();
        format!("ports {}", ports.join(", "))
    };
    format!("sandbox: {access} · {network}")
}

/// The provider of `model` and where its credentials come from.
fn auth_line(model: &str, credential: Option<(AdapterKind, CredentialSource)>) -> BannerLine {
    match credential {
        Some((kind, source)) => {
            let text = format!("{} · {source}", provider_name(kind));
            if source == CredentialSource::Missing {
                BannerLine::warning(text)
            } else {
                BannerLine::new(text)
            }
        }
        None => BannerLine::warning(format!("{model}: unknown provider")),
    }
}

fn context_line(agents: &[PathBuf]) -> String {
    if agents.is_empty() {
        return format!("context: no {}", harness_core::AGENTS_FILE);
    }
    let paths: Vec<_> = agents.iter().map(|p| tilde(p)).collect();
    format!("context: {}", paths.join(", "))
}

fn session_line(session: Option<&Session>, messages: usize) -> String {
    match session {
        None => "sessions off".to_owned(),
        Some(session) if messages > 0 => format!(
            "continued session {} · {messages} messages",
            session.short_id()
        ),
        Some(_) => "new session".to_owned(),
    }
}

fn current_folder() -> String {
    std::env::current_dir().map_or_else(|_| ".".to_owned(), |cwd| tilde(&cwd))
}

/// `path` with `$HOME` shown as `~`.
fn tilde(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        if let Ok(rel) = path.strip_prefix(&home) {
            return if rel.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", rel.display())
            };
        }
    }
    path.display().to_string()
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
        Event::Retry {
            attempt,
            delay,
            reason,
        } => {
            // End any line the failed attempt left half printed.
            println!();
            eprintln!("{}", retry_notice(attempt, delay, reason));
        }
    }
}
