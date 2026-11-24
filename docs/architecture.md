# Architecture

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
`ask_user`, `web_fetch` and `web_search` run there, as they need the user and
the network, and `skill`, which only reads the `SKILL.md` files of
`~/.kluis/skills` found at start.

The agent is a plain state machine. It keeps the conversation as a
`ChatRequest`, reports progress through a borrowed `Event` callback, and runs
tool calls through an `Executor`, local or sandboxed. This keeps the UI and
persistence out of the core loop.

A subagent, started by the `task` tool, is a second agent with an empty
conversation that shares the `Executor` of the first: its tool calls run in
the same worker and sandbox. Its history is dropped once it answered, and
only the answer joins the conversation that is saved.

## Design choices

- Linux only, sandboxed by default.
- Tool results are bounded JSON, so one noisy command cannot flood the model
  context.
- Sessions are JSONL files, one workspace folder per directory, appended to
  and rewritten on compaction.
- The TUI state machine is decoupled from terminal I/O and tested directly.
- Credentials are never passed into the sandbox, and `~/.kluis` is hidden from
  sandboxed tools.
