# K-Luis

![K-Luis logo](logo.png)

> Simplicity-focused AI harness with a fully configurable native sandbox.

[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-9cf)](https://www.kernel.org/)

<!-- TODO: record a 10-second GIF of `kl "fix the failing tests"` and
     replace this with: ![K-Luis demo](demo.gif) -->

## Why K-Luis

Most coding agents either interrupt you with permission prompts or run
whatever the model asks directly on the host. K-Luis is sandboxed by default:

- Choose exactly which folders and ports the model can touch.
- Model commands run in a `hakoniwa` container, not on your machine.
- `.git` stays read-only, so hooks and config cannot run code as you.
- API keys and `~/.kluis` are never exposed to the sandbox.
- No network unless you explicitly allow it.

Prompt injection stops being a disaster: the worst a malicious instruction can
do is mess with the folders you gave it.

## Quick start

```sh
cargo install --path harness-tui

export DEEPSEEK_API_KEY="sk-..."
kl "fix the typo in README.md"   # one task, one command
kl                                # interactive chat, Ctrl-D to quit
kl --login anthropic             # use a Claude subscription instead
kl --resume                       # continue a previous session
```

Want more control? `kl --strict` makes the project read-only, and
`kl --no-sandbox` turns the container off.

## Configuration

The configuration file selects which parts of the container to expose or
deny to the model.

```toml
[generic]
model = "deepseek-chat"
thinking = "medium"

[sandbox]
read = ["/etc/ssl"]
write = ["~/.cache/sccache"]
deny = ["~/.ssh", "~/.gnupg"]
connect = [11434]
network = false
max_processes = 1024
tmp_size = "512M"
max_memory = "4G"
env = ["CC", "CFLAGS"]
```

Lists add up with the matching command-line options, and switches turn on when
either the file or the CLI enables them.

### API keys

Provider credentials go in `~/.kluis/auth.toml`, keyed by the lowercase
provider name. Each entry may set `api_key` and/or `endpoint`:

```toml
[deepseek]
api_key = "sk-..."

[anthropic]
api_key = "sk-ant-..."

[openai]
api_key = "sk-..."

# Custom endpoint, for example a local OpenAI-compatible server.
[genai_1]
endpoint = "http://localhost:8000/v1"
```

Keep the file private: `chmod 600 ~/.kluis/auth.toml`. Environment variables
such as `DEEPSEEK_API_KEY` still take precedence, and an OAuth login takes
precedence over `api_key`.

## Features

### Agent

- Streaming replies and reasoning, with configurable thinking effort.
- Rate limits, overloaded servers and dropped connections are retried up to
  five times with exponential backoff (1 s to 16 s), honoring `Retry-After`.
- Read-only tools (`read`, `ls`, `find`, `grep`) run in parallel.
- `AGENTS.md` instructions loaded from `~/.kluis/AGENTS.md`, the repository
  root, and the current folder.
- Live model discovery across configured providers, with a per-provider
  timeout.
- Sessions persisted as append-only JSONL per workspace; continue the latest
  or resume any session by ID.

### Tools

- `read`, `write`, `edit`, `ls`, `find`, `grep`, `bash`, `todo`, `ask_user`,
  `web_fetch`.
- `edit` applies changes atomically and shows a unified diff.
- `bash` streams cleaned output with a timeout and line limits.
- `todo` keeps the plan of a multi-step task.
- `ask_user` asks you a question mid-task, with suggested answers to pick
  from or your own text (interactive mode only).
- `web_fetch` reads a web page as Markdown (or raw with `raw: true`), 50 KB
  at a time. It runs outside the sandbox, so it is offered only with
  `--network` (or `network = true`) or `--no-sandbox`.
- Tool results are bounded JSON, with `{"error": "..."}` responses so the
  model recovers instead of derailing.

### TUI

- Inline terminal UI that keeps native scrollback.
- Live streaming Markdown and reasoning; `Ctrl-O` toggles thinking display.
- Spinner with elapsed time and live `bash` output preview.
- Persistent input history, model and session pickers, and `/copy` of the
  last reply.
- Slash commands: `/model`, `/thinking`, `/resume`, `/copy`, `/new`, `/help`,
  `/quit`.
- Falls back to line mode when stdin or stdout is not a terminal.

### Sandbox

- `hakoniwa` container showing only the configured folders.
- The current folder is writable by default; `.git` stays read-only so hooks
  and `config` cannot run code on the host.
- `--strict` makes the current folder read-only; `--no-sandbox` runs tools on
  the host.
- Network access is off by default; allow specific TCP ports or all network
  access. Landlock enforces the TCP allowlist on Linux 6.7+, and the
  container uses `CLONE_NEWNET` when no network is allowed.
- Private `/tmp`, process/thread limit, virtual memory limit, and chosen
  environment variables passed to commands.

### Auth & config

- Provider API keys from environment variables (via `genai`).
- `~/.kluis/auth.toml` for per-provider `api_key` and `endpoint` (mode `0600`).
- OAuth login to a Claude Pro/Max subscription: `kl --login anthropic`.
- `~/.kluis/config.toml` for the default model, thinking effort, and sandbox
  settings.

## Architecture

The repository is a Cargo workspace with four crates:

| Crate             | Responsibility                                                      |
| ----------------- | ------------------------------------------------------------------- |
| `harness-core`    | Agent loop, tool dispatch, auth, model discovery, sessions, prompts |
| `harness-exec`    | Tool implementations and the async tool executor                    |
| `harness-sandbox` | `hakoniwa` container configuration                                  |
| `harness-tui`     | The `kl` frontend and inline terminal UI                            |

When the sandbox is on, `kl` starts a worker process (`kl --tool-executor`)
inside the container. The host agent and the worker talk over an async NDJSON
protocol on stdio, supporting parallel tool calls and per-call cancellation.
Commands and file access therefore never run in the host process. Only
`ask_user` and `web_fetch` run there, as they need the user and the network.

The agent is a plain state machine: it keeps the conversation as a
`ChatRequest`, reports progress through a borrowed `Event` callback, and runs
tool calls through an `Executor`, local or sandboxed. This keeps the UI and
persistence out of the core loop.

## Design choices

- Linux only, sandboxed by default.
- Tool results are bounded JSON, so one noisy command cannot flood the model
  context.
- Sessions are append-only JSONL files, one workspace folder per directory.
- The TUI state machine is decoupled from terminal I/O and tested directly.
- Credentials are never passed into the sandbox, and `~/.kluis` is hidden from
  sandboxed tools.

## Roadmap

Planned features are tracked in [TODO.md](TODO.md).

---

`cargo install --path harness-tui` and let the model work — safely.
