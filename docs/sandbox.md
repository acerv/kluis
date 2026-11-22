# Sandbox

Model commands run in a [`hakoniwa`](https://github.com/souk4711/hakoniwa)
container that shows only the folders you configure.

## What is isolated

- The current folder is writable. With `--strict` it is read-only, and
  `--no-sandbox` turns the container off.
- `.git` stays read-only, and a new `.git` is disabled after each prompt.
- There is no network by default. You can allow all of it, or some TCP ports
  (Linux 6.7+). Allowed ports are open on every host, not only locally.
- Cloud drives, network shares and credentials in `$HOME` are hidden:
  `~/.ssh`, `~/.aws`, `gh`, registry and AI tool tokens, browser profiles,
  shell history and more. `sandbox.deny` hides additional paths.
- API keys and `~/.kluis` are never passed into the sandbox.
- The kernel attack surface is reduced: no user namespaces, only common socket
  types, no core dumps.
- Processes and `/tmp` are limited. Leftover programs are stopped after each
  prompt, and on low memory commands are killed first.

See [Configuration](configuration.md) for the settings that control this.

## What is not isolated

The sandbox cannot vet what the model writes. Build scripts, Makefiles,
`.cargo/config.toml`, `package.json` scripts and editor settings run as you
when you build, test or open the project. Review the changes like any patch.

Two tools run outside the sandbox because they need the user or the network:
`ask_user` and `web_fetch`. `web_fetch` is offered only with `--network` (or
`network = true`) or `--no-sandbox`.

## Git safety

Your own git, outside the sandbox, runs hooks and some config settings of any
repository it finds, as you. For full protection, set it up once:

```sh
git config --global safe.bareRepository explicit
```

and turn hooks off everywhere (yours too), for example in
`~/.config/environment.d/git-safety.conf` and your shell profile:

```sh
GIT_CONFIG_COUNT=2
GIT_CONFIG_KEY_0=core.hooksPath
GIT_CONFIG_VALUE_0=/dev/null
GIT_CONFIG_KEY_1=core.fsmonitor
GIT_CONFIG_VALUE_1=false
```
