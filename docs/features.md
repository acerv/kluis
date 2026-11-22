# Features

## Agent

- Streaming replies and reasoning, with configurable thinking effort.
- Rate limits, overloaded servers and dropped connections are retried up to
  five times with exponential backoff (1 s to 16 s), honoring `Retry-After`.
- Read-only tools (`read`, `ls`, `find`, `grep`) run in parallel.
- `AGENTS.md` instructions are loaded from `~/.kluis/AGENTS.md`, the
  repository root, and the current folder. Project ones must not be symlinks,
  which could point at files the sandbox hides.
- Live model discovery across configured providers, with a per-provider
  timeout.
- Sessions are persisted as append-only JSONL per workspace. Continue the
  latest or resume any session by ID.

## Tools

`read`, `write`, `edit`, `ls`, `find`, `grep`, `bash`, `todo`, `ask_user` and
`web_fetch`.

- `edit` applies changes atomically and shows a unified diff.
- `bash` streams cleaned output with a timeout and line limits.
- `todo` keeps the plan of a multi-step task.
- `ask_user` asks you a question mid-task, with suggested answers to pick from
  or your own text (interactive mode only).
- `web_fetch` reads a web page as Markdown (or raw with `raw: true`), 24 KB at
  a time. It runs outside the sandbox, so it is offered only with `--network`
  (or `network = true`) or `--no-sandbox`.
- Tool results are bounded JSON, with `{"error": "..."}` responses so the
  model recovers instead of derailing.

## Terminal UI

- Inline terminal UI that keeps native scrollback.
- Live streaming Markdown and reasoning. `Ctrl-O` toggles thinking display.
- Spinner with elapsed time and live `bash` output preview.
- Persistent input history, model and session pickers, and `/copy` of the last
  reply.
- Slash commands: `/model`, `/thinking`, `/resume`, `/compact`, `/copy`, `/new`, `/help`,
  `/quit`.
- Falls back to line mode when stdin or stdout is not a terminal.
