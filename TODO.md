# TODO

## Config file

`[sandbox]` section in `~/.kluis/config.toml`:

- [x] Paths to deny reading (`.ssh`, `.gnupg`, `.pi`, ...)
- [x] Extra mount points, read-only or writable (`/etc/ssl`, ...)
- [x] Memory limit for commands
- [x] Process limit and `/tmp` size
- [x] Environment variables passed to commands
- [x] TCP ports and network access

## Sandbox

- [x] Mount more of `/etc` by default (`alternatives`, `os-release`, ...)
- [ ] Protect `.git` of new `git init`

## Compatibility

- [ ] Ubuntu 23.10+: AppArmor blocks unprivileged user namespaces
- [ ] Linux < 6.7: no Landlock TCP rules without `--network`

## Tools

Same set as pi: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`.

- [x] `read`/`edit`: do not load huge files whole into memory
- [x] `write`/`edit`: atomic writes (temp file + rename, keep mode)
- [x] `edit`: return a diff for the TUI
- [ ] `read`: images as attachments
- [x] `bash`: save the full output to a file when truncated
- [x] Move `path.rs` unit tests to `tests/`

## TUI

`harness-tui` (`kl` binary): inline UI (`ratatui` + `crossterm`), keeping the
scrollback, falling back to line mode when not on a terminal.

- [x] Event loop over keys, agent updates (channel) and the prompt
- [x] Multiline input with history and bracketed paste
- [ ] Markdown rendering with code highlighting
- [x] Status bar: model, sandbox mode, tokens, folder
- [x] Ctrl-C/Esc cancels the prompt, Ctrl-D quits
- [x] Slash commands (`/model`, `/clear`, `/quit`)
- [x] Restore the terminal on panic, handle resize, `NO_COLOR`
- [x] Tests with ratatui's `TestBackend`

## CLI

- [x] Use the TUI on a terminal, line mode otherwise
- [ ] Check the terminal works inside the container
- [ ] Remove `/tmp/hakoniwa-*` left behind when killed

## Tests

- [x] End-to-end CLI test: commands get no network and no API keys
