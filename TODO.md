# TODO

## Sandbox

- [ ] Protect `.git` of new `git init`

## Compatibility

- [ ] Ubuntu 23.10+: AppArmor blocks unprivileged user namespaces
- [ ] Linux < 6.7: no Landlock TCP rules without `--network`

## Tools

- [ ] `read`: images as attachments

## Agent & Session

- [ ] Default system prompt with cwd/OS context and `AGENTS.md` loading (`~/.kluis/AGENTS.md` + workspace)
- [ ] Session persistence (`~/.kluis/sessions/`) with `--continue` / `-c` and `/resume`
- [ ] Auto-compaction when `input_tokens` approaches the model's context limit
- [ ] Model and thinking defaults in `~/.kluis/config.toml`

## TUI

- [ ] Autocomplete popup for slash commands (`/`) and workspace files (`@path`)
- [ ] `@path` expansion: attach or inline referenced files (and images) into the prompt
- [ ] Allow typing and queuing the next prompt while the agent is busy
- [ ] Editor: word movement (`Ctrl-Left`/`Right`, `Alt-B`/`F`), `Alt-D`, undo (`Ctrl-Z`) and `$EDITOR` (`Ctrl-G`)
- [ ] Persistent input history across runs (`~/.kluis/history`)
- [ ] Direct shell shortcut (`!<cmd>`) running inside the sandbox
- [ ] Live `bash` stdout/stderr preview and elapsed timer on the spinner (`⠋ Running bash… 12s`)
- [ ] Show hunk line numbers in `edit` diffs and expand the last tool output (`Ctrl-O`)
- [ ] Multi-line live streaming preview above the spinner
- [ ] Thinking effort (`off`, `low`, `medium`, `high`) via `/thinking`, keybinding and CLI/config
- [ ] Session commands: `/compact`, `/undo`, `/retry`, `/diff`, `/copy`, `/status`, `/export`, `/resume`
- [ ] Context window usage (`<tokens>/<limit>`) for known models in the status bar
- [ ] Update terminal title with activity state and ring bell on long turns
- [ ] Restore the terminal on `SIGTERM`/`SIGHUP`

## Nice to have

- [ ] Check the terminal works inside the container
