// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Terminal coding agent with native sandboxing.
//!
//! ```sh
//! kl                           # interactive chat, Ctrl-D to quit
//! kl "fix the typo in README.md"   # single prompt
//! git diff | kl -p "review this"    # single prompt, stdin appended
//! kl --output-format json "list the TODOs"   # single prompt, JSON result
//! kl --output-format stream "fix the tests"  # one JSON line per event
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

use clap::{Parser, ValueEnum};
use genai::adapter::AdapterKind;
use genai::chat::ChatMessage;
use harness_core::oauth::flow::Login;
use harness_core::oauth::{OAuthProvider, OAuthStore};
use harness_core::tools::{
    run_worker, Engine, Executor, OutputFiles, Searcher, ToolContext, WebClient,
};
use harness_core::{
    provider_name, Agent, AuthConfig, Command, CredentialSource, Error, Event, Installed,
    SessionData, SessionStore, SessionSummary, Skill, Skills, Thinking, TokenUsage,
};
use harness_sandbox::{GitGuard, NetworkAccess};
use harness_tui::{
    after_commands, printable, pruned_notice, retry_notice, summary, BannerLine, History,
    PromptSaver, PromptStats, RunOptions, Session, Signals, Status, TRUNCATED_NOTICE,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::runtime::Runtime;

/// Largest standard input `--print` adds to the prompt.
const MAX_STDIN_BYTES: u64 = 4 << 20;

/// Terminal coding agent with native sandboxing.
#[derive(Parser)]
#[command(version)]
pub struct Args {
    /// Model to use; the provider is picked from its name [default: deepseek-chat].
    #[arg(long, env = "HARNESS_MODEL")]
    model: Option<String>,

    /// Thinking effort: default, off, low, medium, high, xhigh, or max.
    #[arg(long, env = "HARNESS_THINKING", value_name = "LEVEL")]
    thinking: Option<Thinking>,

    /// Colors of code blocks: a built-in theme, e.g. solarized-dark, or a
    /// TextMate theme file [default: base16-ocean-dark].
    #[arg(long, value_name = "NAME|PATH")]
    code_theme: Option<String>,

    /// Auto-compact conversation when prompt tokens reach this threshold
    /// (0 disables) [default: 100000].
    #[arg(long, env = "HARNESS_AUTO_COMPACT_TOKENS", value_name = "TOKENS")]
    auto_compact_tokens: Option<u64>,

    /// Ring the terminal bell when a prompt that ran for 10 seconds or more
    /// ends or asks a question.
    #[arg(long)]
    notify: bool,

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

    /// Also allow reading this path (repeatable). $HOME is empty but for
    /// the allowed paths and the current folder.
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

    /// Let web_fetch reach only this host and its subdomains, private
    /// addresses included, even without --network (repeatable).
    #[arg(long, value_name = "HOST")]
    web_host: Vec<String>,

    /// Search the web with this SearXNG instance instead of DuckDuckGo.
    #[arg(long, value_name = "URL")]
    searxng_url: Option<String>,

    /// Processes and threads the harness and its commands may run at once
    /// [default: 1024].
    #[arg(long, value_name = "NUM", value_parser = clap::value_parser!(u64).range(1..))]
    max_processes: Option<u64>,

    /// Size of the private /tmp in bytes, or with a K, M or G suffix (a
    /// quarter of the memory by default).
    #[arg(long, value_name = "SIZE", value_parser = config::parse_size)]
    tmp_size: Option<u64>,

    /// Virtual memory limit for commands in bytes, or with a K, M or G suffix.
    #[arg(long, value_name = "SIZE", value_parser = config::parse_size)]
    max_memory: Option<u64>,

    /// Let the model keep commands running in the background across
    /// prompts, such as a server. A git repository they create may be
    /// used by your git before kl disables it.
    #[arg(long)]
    background_commands: bool,

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

    /// Print an AppArmor profile that lets kl create its sandbox on Ubuntu,
    /// then exit.
    #[arg(long, conflicts_with_all = ["login", "logout", "prompt"])]
    apparmor_profile: bool,

    /// Install the skills that come with kl in ~/.kluis/skills, or update
    /// the ones you did not change, then exit.
    #[arg(long, conflicts_with_all = ["login", "logout", "apparmor_profile", "prompt"])]
    install_skills: bool,

    /// Internal mode: run the NDJSON tool worker inside the sandbox container.
    #[arg(long, hide = true)]
    tool_executor: bool,

    /// Internal: the network the tool worker's commands get (none, ports or
    /// any), which decides the sockets they may create.
    #[arg(long, hide = true, requires = "tool_executor", default_value = "none")]
    network_access: NetworkAccess,

    /// Run the prompt once and exit, with standard input appended to it
    /// when it is not a terminal.
    #[arg(short, long)]
    print: bool,

    /// How a prompt run once writes its output: `text`; `json`, one object
    /// with `result`, `usage` and `session`, or `error`; or `stream`, one
    /// JSON line per event (text, tool calls and results, usage, retries)
    /// ending with that object. `json` and `stream` imply --print.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    /// Prompt to run once; starts an interactive chat when absent.
    prompt: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Stream,
}

fn main() -> ExitCode {
    match launch(Args::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {}", printable(&err.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn launch(mut args: Args) -> Result<ExitCode, Error> {
    // Root could undo the sandbox; also kept out of `--no-sandbox`, where
    // commands would act as root on the whole host.
    harness_sandbox::refuse_root()?;
    if args.apparmor_profile {
        print!("{}", sandbox::apparmor_profile()?);
        return Ok(ExitCode::SUCCESS);
    }
    if args.install_skills {
        return install_skills();
    }
    let mut below_init = false;
    if args.tool_executor {
        // When memory runs out, the kernel kills commands before the
        // user's own programs. While `/proc/self` is still ours to write.
        harness_sandbox::kill_first_on_oom()?;
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
        runtime.block_on(run_worker(
            cwd,
            args.max_memory,
            args.background_commands,
            below_init,
        ))?;
    } else if let Some(provider) = args.login {
        runtime.block_on(login(provider))?;
    } else if let Some(provider) = args.logout {
        runtime.block_on(logout(provider))?;
    } else {
        let config_model = apply_config(&mut args)?;
        return run_until_signal(runtime, args, config_model);
    }
    Ok(ExitCode::SUCCESS)
}

/// Install the bundled skills and tell what was done with each one.
fn install_skills() -> Result<ExitCode, Error> {
    let dir = config::skills_path().ok_or("cannot install skills: HOME is not set")?;
    let mut code = ExitCode::SUCCESS;
    for (name, res) in harness_core::install_skills(&dir)? {
        match res {
            Ok(Installed::Added) => println!("Added {name}."),
            Ok(Installed::Updated) => println!("Updated {name}."),
            Ok(Installed::Current) => println!("{name} is up to date."),
            Ok(Installed::Kept) => eprintln!(
                "warning: kept {name}, which you changed; the new version is in {}",
                dir.join(name)
                    .join(format!("{}.new", harness_core::SKILL_FILE))
                    .display()
            ),
            Err(err) => {
                eprintln!("error: {name}: {}", printable(&err.to_string()));
                code = ExitCode::FAILURE;
            }
        }
    }
    Ok(code)
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
    args.code_theme = args.code_theme.take().or(config.generic.code_theme);
    args.notify |= config.generic.notify;
    args.auto_compact_tokens = args
        .auto_compact_tokens
        .or(config.generic.auto_compact_tokens);
    args.allow_read.extend(config.sandbox.read);
    args.allow_write.extend(config.sandbox.write);
    args.deny_read.extend(config.sandbox.deny);
    args.allow_connect.extend(config.sandbox.connect);
    args.searxng_url = args.searxng_url.take().or(config.generic.searxng_url);
    args.network |= config.sandbox.network;
    args.web_host.extend(config.sandbox.web_hosts);
    args.background_commands |= config.sandbox.background_commands;
    args.max_processes = args.max_processes.or(config.sandbox.max_processes);
    args.tmp_size = args.tmp_size.or(config.sandbox.tmp_size);
    args.max_memory = args.max_memory.or(config.sandbox.max_memory);
    args.pass_env.extend(config.sandbox.env);
    args.config = Some(path);
    Ok(config.generic.model)
}

/// [`run`] on `runtime`, leaving with `128 + signal` when a signal stopped it.
fn run_until_signal(
    runtime: Runtime,
    args: Args,
    config_model: Option<String>,
) -> Result<ExitCode, Error> {
    let signals = {
        let _runtime = runtime.enter();
        Signals::listen()?
    };
    let res = runtime.block_on(run(args, config_model, signals.clone()));
    let Some(signal) = signals.received() else {
        return res;
    };
    if let Err(err) = res {
        eprintln!("error: {}", printable(&err.to_string()));
    }
    // A read of standard input left waiting would never end.
    runtime.shutdown_background();
    Ok(ExitCode::from(128 + signal as u8))
}

async fn run(
    mut args: Args,
    config_model: Option<String>,
    signals: Signals,
) -> Result<ExitCode, Error> {
    let cwd = std::env::current_dir()?;
    if args.print || args.output_format != OutputFormat::Text {
        args.prompt = vec![read_prompt(&args).await?];
    }
    // A wrong theme fails now, not only once the terminal UI starts.
    let code_theme = args
        .code_theme
        .as_deref()
        .map(harness_tui::code_theme)
        .transpose()?;
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
    let skills = find_skills(&args, &cwd);
    let mut commands = config::commands_dir()
        .map(|dir| harness_core::find_commands(&dir))
        .unwrap_or_default();
    commands
        .warnings
        .extend(clash_warnings(&mut commands.commands, &skills.skills));
    let system = harness_core::system_prompt(&cwd, &agents, &skills.skills);
    let default_model = config_model.as_deref().unwrap_or(config::DEFAULT_MODEL);
    let Opened {
        model,
        history,
        mut session,
        picker_entries,
    } = open_session(&args, &cwd, default_model)?;
    let credential = AdapterKind::from_model(&model)
        .ok()
        .map(|kind| (kind, auth.credential_source(kind)));
    drop(auth);

    let (executor, mut git_guard) = executor(&args, &cwd)?;
    let agent = Agent::with_client(client, &model)
        .with_oauth(oauth)
        .with_thinking(args.thinking.unwrap_or_default())
        .with_system(system)
        .with_history(history)
        .with_executor(executor)
        .with_skills(skills.skills);
    let mut agent = with_options(agent, &args)?;

    let warnings = startup_warnings(&args, &cwd, skills.warnings, commands.warnings);
    let full_screen =
        args.prompt.is_empty() && io::stdin().is_terminal() && io::stdout().is_terminal();
    if !full_screen {
        for warning in &warnings {
            eprintln!("warning: {warning}");
        }
    }

    if !args.prompt.is_empty() {
        let text = args.prompt.join(" ");
        let text = harness_core::expand_command(&commands.commands, &text).unwrap_or(text);
        return run_once(
            &mut agent,
            session.as_mut(),
            git_guard.as_mut(),
            signals,
            &text,
            args.output_format,
        )
        .await;
    }

    let sandbox = sandbox_label(&args);
    if !full_screen {
        return run_lines(
            agent,
            session,
            git_guard,
            signals,
            &commands.commands,
            &sandbox,
        )
        .await;
    }
    let status = Status {
        sandbox,
        folder: current_folder(),
    };
    let mut banner = vec![
        BannerLine::new(format!("kluis v{}", env!("CARGO_PKG_VERSION"))),
        auth_line(&model, credential),
        BannerLine::new(sandbox_details(&args, &cwd)),
        BannerLine::new(context_line(&agents, &commands.commands, agent.skills())),
        BannerLine::new(session_line(session.as_ref(), agent.history().len())),
    ];
    banner.extend(warnings.into_iter().map(BannerLine::warning));
    let history = config::history_path().map(History::load);
    harness_tui::run(
        agent,
        status,
        signals,
        RunOptions {
            session,
            model: args.model,
            history,
            picker_entries,
            adapters,
            banner,
            git_guard,
            code_theme,
            commands: commands.commands,
            notify: args.notify,
        },
    )
    .await?;
    Ok(ExitCode::SUCCESS)
}

/// The prompt of `-p`: the arguments, then what standard input holds
/// unless it is a terminal.
async fn read_prompt(args: &Args) -> Result<String, Error> {
    let mut text = args.prompt.join(" ");
    if !io::stdin().is_terminal() {
        // Bounded: more could not fit in a request anyway.
        let mut input = String::new();
        tokio::io::stdin()
            .take(MAX_STDIN_BYTES + 1)
            .read_to_string(&mut input)
            .await?;
        if input.len() as u64 > MAX_STDIN_BYTES {
            return Err(format!(
                "standard input is larger than {} MiB",
                MAX_STDIN_BYTES >> 20
            )
            .into());
        }
        if !text.is_empty() && !input.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&input);
    }
    if text.trim().is_empty() {
        return Err("no prompt: give one as arguments or on standard input".into());
    }
    Ok(text)
}

/// The skills of the user, unless commands cannot read them: a folder
/// shown that holds `~/.kluis` keeps `~/.kluis/skills` hidden.
fn find_skills(args: &Args, cwd: &Path) -> Skills {
    let skills_dir = config::skills_dir();
    let hidden_skills = skills_dir
        .as_ref()
        .filter(|_| !args.no_sandbox && sandbox::hides_skills(args, cwd));
    let mut skills = match skills_dir.as_deref() {
        Some(dir) if hidden_skills.is_none() => harness_core::find_skills(dir),
        _ => Skills::default(),
    };
    if let Some(dir) = hidden_skills {
        skills.warnings.push(format!(
            "the skills in {} are not used: a folder shown to commands holds {}, \
             which stays hidden. Show narrower folders instead",
            tilde(dir),
            tilde(dir.parent().unwrap_or(dir)),
        ));
    }
    skills
}

/// The session a run starts with.
struct Opened {
    model: String,
    history: Vec<ChatMessage>,
    session: Option<Session>,
    /// The sessions to pick one from first, for `--resume` without an id.
    picker_entries: Vec<SessionSummary>,
}

/// Open the session `args` ask for in `cwd`: the last one, the one named,
/// or a new one using `default_model` unless `args` name a model.
fn open_session(args: &Args, cwd: &Path, default_model: &str) -> Result<Opened, Error> {
    let store = (!args.no_session)
        .then(config::sessions_dir)
        .flatten()
        .map(|root| SessionStore::new(root, cwd));
    let model = args.model.as_deref().unwrap_or(default_model).to_owned();
    let new = |store: Option<SessionStore>, picker_entries| Opened {
        session: store.map(|store| {
            let writer = store.create(&model);
            Session::new(store, writer, TokenUsage::default(), 0)
        }),
        model: model.clone(),
        history: Vec::new(),
        picker_entries,
    };
    if args.continue_session {
        let store = store.ok_or("cannot continue session: HOME is not set")?;
        let Some(data) = store.latest()? else {
            return Err(format!("no session to continue for {}", cwd.display()).into());
        };
        return open_loaded_session(store, data, args.model.as_deref());
    }
    let Some(query) = &args.resume_session else {
        return Ok(new(store, Vec::new()));
    };
    let store = store.ok_or("cannot resume session: HOME is not set")?;
    if !query.is_empty() {
        let data = store.load(query)?;
        return open_loaded_session(store, data, args.model.as_deref());
    }
    if !args.prompt.is_empty() || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "--resume without an ID requires an interactive terminal (pass --resume <id> or --continue)"
                .into(),
        );
    }
    let entries = store.list()?;
    if entries.is_empty() {
        return Err(format!("no sessions to resume for {}", cwd.display()).into());
    }
    Ok(new(Some(store), entries))
}

/// What runs the tools for `args` in `cwd`: the sandbox, with the guard
/// against the git repositories commands create, or this process.
fn executor(args: &Args, cwd: &Path) -> Result<(Executor, Option<GitGuard>), Error> {
    if !args.no_sandbox {
        let (executor, guard) = sandbox::executor(args, cwd)?;
        return Ok((executor, Some(guard)));
    }
    // Even without the container, keep local bash commands away from API
    // keys in our memory and environment.
    harness_sandbox::protect_self()?;
    let mut tools = ToolContext::default();
    tools.bash.env = Some(harness_sandbox::command_env(
        std::env::vars_os(),
        &args.pass_env,
    ));
    // In the real `/tmp` here: gone with the agent.
    tools.bash.output_files = Some(OutputFiles::default());
    tools.bash.background = args.background_commands;
    Ok((Executor::Local(tools), None))
}

/// `agent` with what `args` turn on: background commands, the size to
/// compact at, and the web tools.
fn with_options(mut agent: Agent, args: &Args) -> Result<Agent, Error> {
    if args.background_commands {
        agent = agent.with_background_commands();
    }
    if let Some(tokens) = args.auto_compact_tokens {
        agent = agent.with_auto_compact(Some(tokens));
    }
    // Fetching and searching run on the host: only when the model may reach
    // the network anyway, which is always the case without the sandbox, or,
    // for fetching, the user lists the hosts it may reach.
    let any_network = args.network || args.no_sandbox;
    if any_network || !args.web_host.is_empty() {
        agent = agent.with_web_fetch(WebClient::new(&args.web_host)?);
    }
    if any_network {
        let searcher = match &args.searxng_url {
            Some(url) => Searcher::new(Engine::Searxng, url)?,
            None => Searcher::duckduckgo()?,
        };
        agent = agent.with_web_search(searcher);
    }
    Ok(agent)
}

/// What to tell the user before the first prompt: the warnings found
/// with the `skills` and the `commands`, and what the sandbox of `args`
/// leaves open in `cwd`.
fn startup_warnings(
    args: &Args,
    cwd: &Path,
    skills: Vec<String>,
    commands: Vec<String>,
) -> Vec<String> {
    // Commands can write a repository only in a writable folder.
    let writable = !args.no_sandbox && (!args.strict || !args.allow_write.is_empty());
    let mut warnings = if writable {
        sandbox::git_warnings(cwd)
    } else {
        Vec::new()
    };
    warnings.extend(skills);
    warnings.extend(commands);
    if !args.no_sandbox {
        warnings.extend(sandbox::secrets_warning(args, cwd));
        warnings.extend(sandbox::remote_warnings(args, cwd));
    }
    warnings
}

/// Run the one prompt `text` and print its result as `format` asks.
async fn run_once(
    agent: &mut Agent,
    mut session: Option<&mut Session>,
    git_guard: Option<&mut GitGuard>,
    mut signals: Signals,
    text: &str,
    format: OutputFormat,
) -> Result<ExitCode, Error> {
    let output: fn(Event<'_>) = match format {
        OutputFormat::Text => print_event,
        OutputFormat::Json => |_| {},
        OutputFormat::Stream => harness_tui::json_lines::print_event,
    };
    let res = prompt_and_persist(
        agent,
        session.as_deref_mut(),
        git_guard,
        &mut signals,
        text,
        output,
        false,
    )
    .await;
    if format == OutputFormat::Text {
        return res.map(|_| ExitCode::SUCCESS);
    }
    let failed = res.is_err();
    let mut out = match res {
        Ok(done) => serde_json::json!({
            "result": done.text,
            "usage": harness_tui::json_lines::usage_json(done.usage),
        }),
        Err(err) => serde_json::json!({ "error": err.to_string() }),
    };
    out["type"] = if failed { "error" } else { "result" }.into();
    out["session"] = session.map(|session| session.current_id()).into();
    println!("{out}");
    Ok(exit_code(failed))
}

/// Run the prompts read from standard input, a line each, until it ends
/// or one of `signals` asks to stop.
async fn run_lines(
    mut agent: Agent,
    mut session: Option<Session>,
    mut git_guard: Option<GitGuard>,
    mut signals: Signals,
    commands: &[Command],
    sandbox: &str,
) -> Result<ExitCode, Error> {
    println!(
        "kluis ({} ({}), {sandbox}), Ctrl-D to quit",
        agent.model(),
        agent.thinking()
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut failed = false;
    loop {
        print!("> ");
        io::stdout().flush()?;

        let line = tokio::select! {
            biased;
            _ = signals.recv() => None,
            line = lines.next_line() => line?,
        };
        let Some(line) = line else {
            println!();
            for notice in after_commands(&agent, git_guard.as_mut(), false, false).await {
                eprintln!("{}", printable(&notice));
            }
            // Scripts feeding prompts learn that one failed.
            return Ok(exit_code(failed));
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let expanded = harness_core::expand_command(commands, line);
        let line = expanded.as_deref().unwrap_or(line);

        // Keep the session alive on failures such as network errors.
        let res = prompt_and_persist(
            &mut agent,
            session.as_mut(),
            git_guard.as_mut(),
            &mut signals,
            line,
            print_event,
            true,
        )
        .await;
        if let Err(err) = res {
            eprintln!("error: {}", printable(&err.to_string()));
            failed = true;
        }
    }
}

fn exit_code(failed: bool) -> ExitCode {
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn open_loaded_session(
    store: SessionStore,
    data: SessionData,
    model_override: Option<&str>,
) -> Result<Opened, Error> {
    let model = model_override.unwrap_or(&data.meta.model).to_owned();
    let mut writer = store.open(&data);
    if model_override.is_some() && model != data.meta.model {
        writer.append_model(&model)?;
    }
    Ok(Opened {
        model,
        session: Some(Session::new(store, writer, data.usage, data.context)),
        history: data.messages,
        picker_entries: Vec::new(),
    })
}

/// What a prompt left: the last text of the model, and the tokens it used.
struct Done {
    text: String,
    usage: TokenUsage,
}

/// Run a prompt, passing its events to `output`, and save it; one of
/// `signals` stops it, which is an error. Commands started in the
/// background keep running with `keep_jobs`, unless a signal came.
async fn prompt_and_persist(
    agent: &mut Agent,
    session: Option<&mut Session>,
    git_guard: Option<&mut GitGuard>,
    signals: &mut Signals,
    text: &str,
    output: fn(Event<'_>),
    keep_jobs: bool,
) -> Result<Done, Error> {
    let mut saver = PromptSaver::new(session, agent.history());
    let mut stats = PromptStats::default();
    let prompt = agent.prompt(text, |event| {
        saver.on_event(&event);
        // The summary of a compaction is not an answer to print.
        let summary = stats.compacting() && matches!(event, Event::TextDelta(_) | Event::Text(_));
        stats.on_event(&event);
        if !summary {
            output(event);
        }
    });
    let res = tokio::select! {
        res = prompt => res,
        signal = signals.recv() => Err(format!("stopped by signal {signal}").into()),
    };
    let keep_jobs = keep_jobs && signals.received().is_none();
    let notices = after_commands(agent, git_guard, stats.changed_files, keep_jobs).await;
    if let Err(err) = saver.finish(agent.history(), stats.usage, stats.context) {
        eprintln!("warning: could not save session: {err}");
    }
    for notice in notices {
        eprintln!("{}", printable(&notice));
    }
    res.map(|_| Done {
        text: stats.text,
        usage: stats.usage,
    })
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
    let mut details = format!("sandbox: {access} · {network}");
    if !args.web_host.is_empty() {
        details += &format!(" · web {}", args.web_host.join(", "));
    }
    details
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

fn context_line(agents: &[PathBuf], commands: &[Command], skills: &[Skill]) -> String {
    let mut line = if agents.is_empty() {
        format!("context: no {}", harness_core::AGENTS_FILE)
    } else {
        let paths: Vec<_> = agents.iter().map(|p| tilde(p)).collect();
        format!("context: {}", paths.join(", "))
    };
    for (count, what) in [(commands.len(), "command"), (skills.len(), "skill")] {
        match count {
            0 => {}
            1 => line.push_str(&format!(" · 1 {what}")),
            n => line.push_str(&format!(" · {n} {what}s")),
        }
    }
    line
}

/// Drop the custom commands a command of kl hides, returning warnings
/// about them and about the skills a command hides.
fn clash_warnings(commands: &mut Vec<Command>, skills: &[Skill]) -> Vec<String> {
    let mut warnings = Vec::new();
    commands.retain(|Command { name, .. }| {
        let builtin = harness_tui::is_builtin(name);
        if builtin {
            warnings.push(format!(
                "the command {name} is not used: /{name} is a command of kl"
            ));
        }
        !builtin
    });
    for skill in skills {
        let name = &skill.name;
        if harness_tui::is_builtin(name) {
            warnings.push(format!(
                "/{name} runs the command of kl, not the skill {name}"
            ));
        } else if commands.iter().any(|command| command.name == *name) {
            warnings.push(format!(
                "/{name} runs the custom command {name}, not the skill {name}"
            ));
        }
    }
    warnings
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
            print!("{}", printable(text));
            // Show the text as it comes, not line by line.
            let _ = io::stdout().flush();
        }
        // Already printed piece by piece: end its line.
        Event::Text(_) => println!(),
        Event::Messages(_)
        | Event::Steered(_)
        | Event::TurnStart
        | Event::ReasoningDelta(_)
        | Event::ToolOutput(_)
        | Event::TurnEnd(_)
        | Event::TaskUsage(_) => {}
        Event::Truncated => eprintln!("{TRUNCATED_NOTICE}"),
        Event::ToolCall(call) => println!(
            "→ {} {}",
            printable(&call.fn_name),
            printable(&summary(&call.fn_arguments.to_string()))
        ),
        Event::ToolResult(resp, _) => println!("← {}", printable(&summary(&resp.content))),
        Event::Retry {
            attempt,
            delay,
            reason,
        } => {
            // End any line the failed attempt left half printed.
            println!();
            eprintln!("{}", printable(&retry_notice(attempt, delay, reason)));
        }
        Event::Compacting { context_tokens } => {
            eprintln!("[auto-compacting ({context_tokens} tokens)…]");
        }
        Event::Compacted { before, .. } => {
            eprintln!("[compacted {before} messages]");
        }
        Event::CompactFailed(err) => {
            eprintln!("[auto-compact failed: {}]", printable(err));
        }
        Event::Pruned(count) => eprintln!("{}", pruned_notice(count)),
    }
}
