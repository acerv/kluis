# TODO

## Config file

`[sandbox]` section in `~/.config/harness/config.toml`:

- [ ] Paths to deny reading (`.ssh`, `.gnupg`, `.pi`, ...)
- [ ] Extra mount points, read-only or writable (`/etc/ssl`, ...)
- [ ] Memory limit for commands
- [ ] Process limit and `/tmp` size
- [ ] Environment variables passed to commands
- [ ] TCP ports and network access

## Sandbox

- [ ] Mount more of `/etc` by default (`alternatives`, `os-release`, ...)
- [ ] Protect `.git` of nested repos and of new `git init`

## Compatibility

- [ ] Ubuntu 23.10+: AppArmor blocks unprivileged user namespaces
- [ ] Linux < 6.7: no Landlock TCP rules without `--network`

## Core

- [ ] Stream model text (`exec_chat_stream`, `Event::TextDelta`)
- [ ] Cancel-safe `prompt`: dropping it keeps the history consistent
- [ ] Events for token usage and turn start/end

## Tools

Same set as pi: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`.

- [ ] `read`/`edit`: do not load huge files whole into memory
- [ ] `write`/`edit`: atomic writes (temp file + rename, keep mode)
- [ ] `edit`: return a diff for the TUI
- [ ] `read`: images as attachments
- [ ] `bash`: save the full output to a file when truncated
- [ ] Move `path.rs` unit tests to `tests/`

## TUI

New `harness-tui` library crate: inline UI (`ratatui` + `crossterm`),
keeping the scrollback. `harness-cli` builds the `Agent` and passes it to
`harness_tui::run(agent, status)`.

- [ ] Event loop over keys, agent updates (channel) and the prompt
- [ ] Multiline input with history and bracketed paste
- [ ] Markdown rendering with code highlighting
- [ ] Status bar: model, sandbox mode, tokens, folder
- [ ] Ctrl-C/Esc cancels the prompt, Ctrl-D quits
- [ ] Slash commands (`/model`, `/clear`, `/quit`)
- [ ] Restore the terminal on panic, handle resize, `NO_COLOR`
- [ ] Tests with ratatui's `TestBackend`

## CLI

- [ ] Use the TUI on a terminal, line mode otherwise
- [ ] Check the terminal works inside the container
- [ ] Remove `/tmp/hakoniwa-*` left behind when killed

## Tests

- [ ] End-to-end CLI test: commands get no network and no API keys
