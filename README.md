<p align="center">
  <img src="logo.png" alt="K-Luis logo" width="160">
</p>

<h1 align="center">K-Luis</h1>

<p align="center">
  Simplicity-focused AI harness with a fully configurable native sandbox.
</p>

<p align="center">
  <a href="https://github.com/acerv/kluis/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/acerv/kluis/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: GPL-3.0" src="https://img.shields.io/badge/license-GPL--3.0-blue"></a>
  <a href="https://www.kernel.org/"><img alt="Platform: Linux" src="https://img.shields.io/badge/platform-Linux-9cf"></a>
</p>

> **Note:** K-Luis is an experimental project, born from the idea of studying
> AI integration inside projects and building something useful for my daily
> routine job.

## Why K-Luis

K-Luis is a minimal harness. It has what you need and nothing more:

- Everything a coding agent needs: read, search and edit files, run commands,
  search the web, fetch pages, and ask you questions. See [Features](docs/features.md).
- A small terminal UI with sessions, model switching and streaming replies.
- One short config file, and an option for each setting.
- Any provider supported by `genai`, plus a Claude subscription login.

It also has a good sandbox, on by default. Most coding agents either
interrupt you with permission prompts or run whatever the model asks directly
on the host. K-Luis does neither:

- You choose which folders and ports the model can touch.
- Commands run in a `hakoniwa` container, not on your machine.
- `.git` stays read-only, so hooks and config cannot run code as you.
- API keys and `~/.kluis` are never exposed to the sandbox.
- There is no network unless you allow it.

A malicious instruction in a file or web page can only affect the folders you
gave the model.

## Install

K-Luis needs Linux and a recent stable Rust toolchain. Allowing individual TCP
ports needs Linux 6.7 or later. On Ubuntu 23.10 and later, AppArmor needs a
one-time setup: `kl` prints the commands, see [Ubuntu](docs/sandbox.md#ubuntu).

```sh
cargo install --path harness-tui
export DEEPSEEK_API_KEY="sk-..."
```

## Usage

```sh
kl "fix the typo in README.md"   # one task, one command
git diff | kl -p "review this"   # stdin appended to the prompt
kl --output-format json "list the TODOs"  # result, usage and session as JSON
kl --output-format stream "fix the tests" # one JSON line per event, then the result
kl                               # interactive chat, Ctrl-D to quit
kl --login anthropic             # use a Claude subscription instead
kl --resume                      # continue a previous session
kl --strict                      # make the project read-only
kl --no-sandbox                  # run tools on the host
```

Settings live in `~/.kluis/config.toml`:

```toml
[generic]
model = "ollama::qwen2.5-coder"   # a local model, no API key needed
thinking = "medium"               # default, off, low, medium, high, xhigh or max
code_theme = "solarized-dark"     # colors of code blocks

[sandbox]
read = ["~/.cargo", "~/.rustup"]  # extra folders the model can read; $HOME is empty otherwise
write = ["~/scratch"]             # extra folders the model can change
deny = ["~/work/secrets"]         # folders to hide
connect = [5432]                  # TCP ports commands can reach, e.g. a local PostgreSQL
max_memory = "4G"                 # memory limit per command
```

All settings are listed in [Configuration](docs/configuration.md).

### Local and remote models

Every provider in `~/.kluis/auth.toml` is available at the same time. Add a
local Ollama server next to a remote provider:

```toml
[deepseek]
api_key = "sk-..."

[ollama]
endpoint = "http://localhost:11434"
```

Then pick the model per run, or switch inside a chat with `/model`:

```sh
kl --model ollama::qwen2.5-coder "explain this function"   # local
kl --model deepseek-flash "refactor the parser"             # remote
```

## Security model

The sandbox limits what commands can read, write and connect to. It cannot vet
what the model writes: build scripts, Makefiles, `.cargo/config.toml`,
`package.json` scripts and editor settings run as you when you build, test or
open the project. Review the changes like any patch.

Your own git, outside the sandbox, also runs repository hooks. See
[Sandbox](docs/sandbox.md#git-safety) for how to turn them off.

## Documentation

- [Configuration](docs/configuration.md): settings, options, environment
  variables and API keys
- [Sandbox](docs/sandbox.md): what is isolated, what is not, and git safety
- [Features](docs/features.md): agent, tools and terminal UI
- [Architecture](docs/architecture.md): crates, process model and design
  choices
- [Roadmap](https://github.com/acerv/kluis/issues): planned features

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
