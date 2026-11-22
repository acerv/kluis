# TODO

## Sandbox

- [ ] Running a file of unknown format makes the kernel run `modprobe` for a
  `binfmt-XXXX` module; only a host setting (`kernel.modprobe`,
  `modules_disabled`) can stop it
- [ ] `/proc/interrupts` and `/proc/pressure` show host activity (interrupt
  counts tell when the user types); hakoniwa mounts `/proc` last, so masking
  needs a user namespace of the worker's own
- [ ] Review mode: commands write to a copy of the project, and kl applies
  the changes after each prompt, without `.git`, hooks, editor settings and
  other dangerous files
- [ ] Editor settings (`.zed/`, `.vscode/`, `.idea/`) can run commands when
  the project is opened; only review mode would cover them
- [ ] Repositories not named `.git`: detect them by content (`HEAD`,
  `objects/`, `refs/`), keep them read-only, disable new ones
- [ ] Hooks kept in the project (`core.hooksPath`, e.g. husky): keep that
  folder read-only
- [ ] `kl --setup-git`: set `safe.bareRepository=explicit` after asking
- [ ] Hide `$HOME` by default and show only toolchains, for credentials the
  built-in list misses
- [ ] Warn when the project or `$HOME` is on a FUSE or network filesystem
- [ ] Refuse more unneeded system calls (`keyctl`, `bpf`, `perf_event_open`,
  `userfaultfd`, mount family, `kexec`, module loading)
- [ ] Static `kl` build (musl), so `LD_PRELOAD` cannot run code under its
  AppArmor profile
- [ ] Limit the memory of all commands together with a cgroup
  (`memory.max`), where systemd delegates the memory controller

## Tools

- [ ] `read`: images as attachments
- [ ] Plan mode: the model proposes a plan and the user approves it first
- [ ] Subagents for large tasks
- [ ] Permissions: per-tool `allow`/`ask`/`deny` in `config.toml`, so
  `--no-sandbox` is not unguarded
- [ ] File checkpoints of `edit`/`write` changes, to undo them

## Agent & Session

- [ ] Auto-compaction when `input_tokens` approaches the context limit
- [ ] Recover when the provider rejects an oversized request
- [ ] Limit the turns of a prompt and detect repeated tool calls
- [ ] Steering: add a user message between tool calls of a running turn
- [ ] Cost tracking: token usage as money per session
- [ ] Sessions over `MAX_SESSION_BYTES` can't be resumed; prune old ones
- [ ] Extensibility: MCP servers, custom commands, prompt templates, skills

## TUI

- [ ] `/diff`: show the changes made to the project during the session
- [ ] Autocomplete for slash commands (`/`) and workspace files (`@path`)
- [ ] `@path` expansion: attach referenced files (and images) to the prompt
- [ ] Type and queue the next prompt while the agent is busy
- [ ] Editor: word movement, `Alt-D`, undo (`Ctrl-Z`), `$EDITOR` (`Ctrl-G`)
- [ ] `!<cmd>`: run a shell command inside the sandbox
- [ ] Hunk line numbers in `edit` diffs, and the full tool output
- [ ] Session commands: `/compact`, `/undo`, `/retry`, `/status`, `/export`
- [ ] Context window usage (`<tokens>/<limit>`) in the status bar
- [ ] Terminal title with the activity, and a bell on long turns

## CLI

- [ ] Non-interactive mode: `--print`, JSON output and exit codes
- [ ] Line mode turns all tool call arguments into text to print 80
  characters of them
