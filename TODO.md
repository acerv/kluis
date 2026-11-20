# TODO

## Sandbox

- [ ] Protect `.git` of new `git init`

## Compatibility

- [ ] Ubuntu 23.10+: AppArmor blocks unprivileged user namespaces; at least fail with a clear message and document the setup

## Tools

- [ ] `read`: images as attachments
- [ ] `web_fetch`: let the model read documentation and issues (decide how it fits the sandbox: network is off by default)
- [ ] Plan/todo tool so the model can keep a plan outside the context
- [ ] Subagents for large tasks
- [ ] Per-tool `allow`/`ask`/`deny` permissions in `config.toml`, so `--no-sandbox` is not unguarded
- [ ] File checkpoints of `edit`/`write` changes (git snapshot or per-turn backup) to make them reversible

## Agent & Session

- [ ] Auto-compaction when `input_tokens` approaches the model's context limit
- [ ] Recover when the provider rejects an oversized request (compact and retry)
- [ ] Retry with exponential backoff on transient API errors (rate limit, 5xx, dropped stream), honoring `Retry-After`
- [ ] Limit the turns of a prompt and detect repeated identical tool calls
- [ ] Steering: inject a user message between tool calls of a running turn
- [ ] Cost tracking: convert persisted token usage into money per session
- [ ] Sessions over `MAX_SESSION_BYTES` can't be resumed; add pruning/deletion of old sessions
- [ ] Extensibility: MCP servers, custom slash commands, prompt templates and skills

## TUI

- [ ] Autocomplete popup for slash commands (`/`) and workspace files (`@path`)
- [ ] `@path` expansion: attach or inline referenced files (and images) into the prompt
- [ ] Allow typing and queuing the next prompt while the agent is busy
- [ ] Editor: word movement (`Ctrl-Left`/`Right`, `Alt-B`/`F`), `Alt-D`, undo (`Ctrl-Z`) and `$EDITOR` (`Ctrl-G`)
- [ ] Direct shell shortcut (`!<cmd>`) running inside the sandbox
- [ ] Show hunk line numbers in `edit` diffs and expand full tool output
- [ ] Session commands: `/compact`, `/undo`, `/retry`, `/diff`, `/status` (model, thinking, tokens, cost, session ID), `/export`
- [ ] Context window usage (`<tokens>/<limit>`) for known models in the status bar
- [ ] Update terminal title with activity state and ring bell on long turns

## CLI

- [ ] Non-interactive mode: `--print` / JSON output and meaningful exit codes for scripts and CI

## Docs

- [ ] README: install, sandbox requirements, config, login
