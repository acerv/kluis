# TODO

## Sandbox

- [ ] Protect `.git` of new `git init`
- [x] Count processes per container: `RLIMIT_NPROC` is per user namespace since Linux 5.14

## Compatibility

- [ ] Ubuntu 23.10+: AppArmor blocks unprivileged user namespaces
- [x] Linux < 6.7: no Landlock TCP rules without `--network` (tool container uses `CLONE_NEWNET` by default)

## Tools

- [ ] `read`: images as attachments

## Agent & Session

- [x] Default system prompt with cwd/OS context and `AGENTS.md` loading (`~/.kluis/AGENTS.md` + workspace)
- [x] Session persistence (`~/.kluis/sessions/`) with `--continue` / `-c`
- [x] Session picker with `/resume` and `--resume`
- [ ] Auto-compaction when `input_tokens` approaches the model's context limit
- [x] Fail tool calls of a truncated response, telling the model to re-issue them, instead of running incomplete arguments
- [x] Model and thinking defaults in `~/.kluis/config.toml`
- [x] Provider credentials in `~/.kluis/auth.toml` (`0600`) and interactive `/model` picker with live discovery

## TUI

- [ ] Autocomplete popup for slash commands (`/`) and workspace files (`@path`)
- [ ] `@path` expansion: attach or inline referenced files (and images) into the prompt
- [ ] Allow typing and queuing the next prompt while the agent is busy
- [ ] Editor: word movement (`Ctrl-Left`/`Right`, `Alt-B`/`F`), `Alt-D`, undo (`Ctrl-Z`) and `$EDITOR` (`Ctrl-G`)
- [x] Persistent input history across runs (`~/.kluis/history`)
- [ ] Direct shell shortcut (`!<cmd>`) running inside the sandbox
- [x] Live `bash` stdout/stderr preview and elapsed timer on the spinner (`⠋ Running bash… 12s`)
- [x] Real-time multi-line thinking and text streaming, toggled with `Ctrl-O`
- [ ] Show hunk line numbers in `edit` diffs and expand full tool output
- [x] Thinking effort (`default`, `low`, `medium`, `high`) via `/thinking`, keybinding and CLI/config
- [x] `/copy` command to copy last assistant reply to clipboard
- [ ] Session commands: `/compact`, `/undo`, `/retry`, `/diff`, `/status`, `/export`
- [ ] Context window usage (`<tokens>/<limit>`) for known models in the status bar
- [ ] Update terminal title with activity state and ring bell on long turns
- [x] Restore the terminal on `SIGTERM`/`SIGHUP`
