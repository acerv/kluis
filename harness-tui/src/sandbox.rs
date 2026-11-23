// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Sandboxed tool executor for `kl`: spawns `kl --tool-executor` inside a
//! `hakoniwa` container with the current folder writable (or read-only with
//! `--strict`), `~/.kluis` hidden, zero API keys in the environment, and no
//! network unless `--network` or `--allow-connect` is given. Commands
//! cannot create user namespaces, nor Unix sockets without `--network`,
//! nor change the hooks git on the host runs.

use std::io::Write;
use std::path::{Path, PathBuf};

use harness_core::tools::{Executor, SandboxConfig};
use harness_core::Error;
use harness_sandbox::{apparmor, GitGuard, Options, Sandbox};

use crate::Args;

/// Name of the AppArmor profile of `kl`.
const PROFILE: &str = "kl";

/// Where the AppArmor help suggests to copy `kl`, as only root may change it.
const SYSTEM_DIR: &str = "/usr/local/bin";

/// The section `$anchor` of the sandbox documentation.
macro_rules! sandbox_docs {
    ($anchor:literal) => {
        concat!(
            env!("CARGO_PKG_REPOSITORY"),
            "/blob/main/docs/sandbox.md#",
            $anchor
        )
    };
}

/// The documentation of the AppArmor setup.
const APPARMOR_DOCS: &str = sandbox_docs!("ubuntu");

/// The documentation of the git setup.
const GIT_DOCS: &str = sandbox_docs!("git-safety");

/// The AppArmor profile that lets the running `kl` create its sandbox on
/// Ubuntu, if only root may change it.
pub fn apparmor_profile() -> Result<String, Error> {
    let exe = std::env::current_exe()?;
    apparmor::check_owner(&exe).map_err(|e| {
        format!("{e}\nCopy kl to a folder only root may change first, see {APPARMOR_DOCS}")
    })?;
    Ok(apparmor::profile(PROFILE, &exe)?)
}

/// How to let `exe` create its sandbox when AppArmor blocks it.
fn apparmor_help(exe: &Path) -> String {
    let mut steps = String::new();
    let target = if apparmor::check_owner(exe).is_ok() {
        exe.to_path_buf()
    } else {
        let target = PathBuf::from(SYSTEM_DIR).join(exe.file_name().unwrap_or(PROFILE.as_ref()));
        steps += &format!(
            "  sudo install -m 755 {} {}\n",
            exe.display(),
            target.display()
        );
        target
    };
    let target = target.display();
    format!(
        "AppArmor blocks the user namespaces the sandbox needs.\n\n\
         To allow kl, run once:\n\n\
         {steps}  \
         {target} --apparmor-profile | sudo tee /etc/apparmor.d/{PROFILE} > /dev/null\n  \
         sudo apparmor_parser -r /etc/apparmor.d/{PROFILE}\n\n\
         Then start {target}, or run with --no-sandbox. Details and alternatives:\n\
         {APPARMOR_DOCS}"
    )
}

/// Spawn the sandboxed tool executor for `args` in `cwd`, with the guard
/// against git repositories its commands create.
pub fn executor(args: &Args, cwd: &Path) -> Result<(Executor, GitGuard), Error> {
    let exe = std::env::current_exe()?;
    if apparmor::userns_blocked() {
        return Err(apparmor_help(&exe).into());
    }
    let mut read = args.allow_read.clone();
    let mut write = args.allow_write.clone();
    let mut deny = args.deny_read.clone();
    if args.strict {
        read.push(cwd.to_path_buf());
    } else {
        write.push(cwd.to_path_buf());
    }

    // Hide all harness state (`~/.kluis`: config, auth, sessions, history,
    // global AGENTS.md) from sandboxed tools.
    let kluis_dir = crate::config::kluis_dir();
    if let Some(dir) = &kluis_dir {
        deny.push(dir.clone());
    }

    // If `--config` lives inside a writable folder (outside `~/.kluis`), keep
    // it read-only so commands cannot tamper with it.
    let pin_config = args.config.as_ref().filter(|config| {
        !kluis_dir
            .as_ref()
            .is_some_and(|dir| config.starts_with(dir))
            && inside_any(config, &write)
    });
    // Git on the host runs the hooks there, as the user: keep them as they
    // are, as for a `.git`, when they live in a writable folder.
    let pin_hooks = hooks_dir(cwd).filter(|hooks| hooks.exists() && inside_any(hooks, &write));

    let mut sandbox = Sandbox::with_defaults(&Options {
        read,
        write,
        deny,
        connect: args.allow_connect.clone(),
        network: args.network,
        max_processes: args.max_processes,
        tmp_size: args.tmp_size,
    })
    .allow_read(&exe);
    if let Some(config) = pin_config {
        sandbox = sandbox.keep_read_only(config);
    }
    if let Some(hooks) = pin_hooks {
        sandbox = sandbox.keep_read_only(hooks);
    }

    let mut worker_args = vec!["--tool-executor".to_owned()];
    // Decides the sockets commands may create: no Unix ones, which lead to
    // programs of the host, unless any network is allowed anyway, and no
    // UDP ones when only some TCP ports are.
    let access = if args.network {
        "any"
    } else if args.allow_connect.is_empty() {
        "none"
    } else {
        "ports"
    };
    worker_args.extend(["--network-access".to_owned(), access.to_owned()]);
    if let Some(bytes) = args.max_memory {
        worker_args.push("--max-memory".to_owned());
        worker_args.push(bytes.to_string());
    }

    let env = harness_sandbox::command_env(std::env::vars_os(), &args.pass_env)
        .into_iter()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();

    let git_guard = GitGuard::new(&sandbox)?;
    let executor = Executor::sandbox(SandboxConfig {
        sandbox,
        program: exe,
        args: worker_args,
        cwd: cwd.to_path_buf(),
        env,
    })?;
    Ok((executor, git_guard))
}

/// Whether `path` lies below one of `folders`, as given or resolved.
fn inside_any(path: &Path, folders: &[PathBuf]) -> bool {
    let canon = std::fs::canonicalize(path).ok();
    folders.iter().any(|folder| {
        path.starts_with(folder)
            || canon
                .as_deref()
                .zip(std::fs::canonicalize(folder).ok().as_deref())
                .is_some_and(|(c, cf)| c.starts_with(cf))
    })
}

/// The folder git takes hooks from in `cwd`, after all settings: `None`
/// without git, outside a repository, or when hooks are turned off.
fn hooks_dir(cwd: &Path) -> Option<PathBuf> {
    let path = git(&["rev-parse", "--git-path", "hooks"], cwd)?;
    (!path.is_empty() && path != "/dev/null").then(|| cwd.join(path))
}

/// Warnings about what git on the host would run of what commands write in
/// `cwd`. Nothing without git.
pub fn git_warnings(cwd: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    // Git takes any folder holding `HEAD`, `objects` and `refs` for a
    // repository, and runs what its config names.
    if bare_repository().is_some_and(|value| !value.eq_ignore_ascii_case(EXPLICIT)) {
        warnings.push(
            "git would run repositories that commands create under any name: run \
             `kl --setup-git`"
                .to_owned(),
        );
    }
    if hooks_dir(cwd).is_some() {
        // A repository's own setting, as husky makes, beats the global one.
        let local = git(&["config", "--local", "--get", "core.hooksPath"], cwd);
        let fix = if local.is_some_and(|value| !value.is_empty()) {
            format!("this repository sets its own hooks, so turn them off as in {GIT_DOCS}")
        } else {
            "run `git config --global core.hooksPath /dev/null` to turn hooks off".to_owned()
        };
        warnings.push(format!(
            "git would run hooks of this project, which may run what commands write: {fix}"
        ));
    }
    warnings
}

/// The value of `safe.bareRepository` that makes git use only repositories
/// named `.git`, or given with `--git-dir`.
const EXPLICIT: &str = "explicit";

/// The `safe.bareRepository` git uses: empty when not set, `None` without
/// git. Asked from `/`, so that a repository's own config does not answer:
/// git ignores this setting there.
fn bare_repository() -> Option<String> {
    git(&["config", "--get", "safe.bareRepository"], Path::new("/"))
}

/// Set `safe.bareRepository` to `explicit` in the user's git config, after
/// asking, so that git no longer takes a folder of commands for a repository.
pub fn setup_git() -> Result<(), Error> {
    let mut output = std::io::stdout();
    let current = bare_repository().ok_or("git not found")?;
    if current.eq_ignore_ascii_case(EXPLICIT) {
        writeln!(
            output,
            "git is already set up: safe.bareRepository is {EXPLICIT}."
        )?;
        return Ok(());
    }
    write!(
        output,
        "git takes any folder holding HEAD, objects and refs for a repository, \
         and runs what its config names: commands could create one that \
         runs on the host.\n\
         Run `git config --global safe.bareRepository {EXPLICIT}`? [y/N] "
    )?;
    output.flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        writeln!(output, "Nothing changed.")?;
        return Ok(());
    }
    let status = std::process::Command::new("git")
        .args(["config", "--global", "safe.bareRepository", EXPLICIT])
        .status()?;
    if !status.success() {
        return Err(format!("git config failed ({status})").into());
    }
    writeln!(output, "Done.")?;
    Ok(())
}

/// The first line `git` prints for `args` in `cwd`: empty when it fails,
/// as for a setting that is not set, and `None` without git.
fn git(args: &[&str], cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = stdout.lines().next().unwrap_or_default().trim();
    Some(if out.status.success() { first } else { "" }.to_owned())
}
