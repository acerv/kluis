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
- Sessions are persisted as JSONL per workspace, appended to as they go and
  rewritten without the replaced messages on compaction, so long ones stay
  small enough to resume. Continue the latest or resume any session by ID.
- Automatic conversation compaction when prompt tokens reach the configured
  threshold (default 100K), plus manual `/compact [focus]`.
- When the provider refuses a request as larger than the context window, the
  outputs of old tool calls are dropped from the conversation and the turn is
  asked again.

## Tools

`read`, `write`, `edit`, `ls`, `find`, `grep`, `bash`, `todo`, `ask_user` and
`web_fetch`.

- `read` hands PNG, JPEG, GIF and WebP images (up to 3.5 MB) to the model.
  They are not saved in the session.
- `edit` applies changes atomically and shows a diff with line numbers.
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
- Messages typed while the agent works join the conversation after the
  running tool calls; one sent as the model answers starts the next turn.
  Cancelling puts the ones left back in the editor.
- `!<command>` runs a shell command as the `bash` tool does, in the sandbox
  when the tools run there, without adding it to the conversation.
- `/output` shows the whole output of the last tool call.
- Slash commands: `/model`, `/thinking`, `/resume`, `/copy`, `/output`,
  `/compact`, `/status`, `/new`, `/help`, `/quit`.
- Falls back to line mode when stdin or stdout is not a terminal.
