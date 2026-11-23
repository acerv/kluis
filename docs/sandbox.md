# Sandbox

Model commands run in a [`hakoniwa`](https://github.com/souk4711/hakoniwa)
container that shows only the folders you configure.

## What is isolated

- The current folder is writable. With `--strict` it is read-only, and
  `--no-sandbox` turns the container off.
- `.git` stays read-only, and a new `.git` is disabled after each prompt.
- There is no network by default. You can allow all of it, or some TCP ports
  (Linux 6.7+). Allowed ports are open on every host, not only locally.
- `$HOME` is an empty private folder that lives in memory and is gone when
  `kl` exits. Only the current folder and the `sandbox.read` and
  `sandbox.write` paths below it show there. See [Home folder](#home-folder).
- Cloud drives, network shares and credentials in `$HOME` are hidden, even
  inside a folder you show: `~/.ssh`, `~/.aws`, `gh`, registry and AI tool
  tokens, browser profiles, shell history and more. `sandbox.deny` hides
  additional paths.
- API keys and `~/.kluis` are never passed into the sandbox.
- The kernel attack surface is reduced: no user namespaces, only common socket
  types, no core dumps.
- Processes and `/tmp` are limited. Leftover programs are stopped after each
  prompt, and on low memory commands are killed first.

See [Configuration](configuration.md) for the settings that control this.

## Home folder

Commands see `$HOME` at the usual path, but it starts empty and nothing
written there reaches your real home folder. Tools installed in your home,
such as Rust from rustup, are missing until you show them:

```toml
[sandbox]
read = ["~/.cargo", "~/.rustup", "~/.local/bin", "~/.gitconfig"]
write = ["~/.cache/sccache"]
```

`--allow-read` and `--allow-write` do the same for one run. When a folder
you show holds credentials `kl` knows about, such as `~/.config/gh` below
`~/.config`, they stay hidden and `kl` names them in a warning at start:
show narrower folders instead.

Showing `~` itself, as `read = ["~"]`, brings back the whole home folder
read-only, with the credentials still hidden.

## What is not isolated

The sandbox cannot vet what the model writes. Build scripts, Makefiles,
`.cargo/config.toml`, `package.json` scripts and editor settings run as you
when you build, test or open the project. Review the changes like any patch.

Two tools run outside the sandbox because they need the user or the network:
`ask_user` and `web_fetch`. `web_fetch` is offered only with `--network` (or
`network = true`) or `--no-sandbox`.

## Git safety

Your own git, outside the sandbox, runs hooks and some config settings of any
repository it finds, as you. `kl` warns at start when git would run hooks of
the project, or repositories that commands create under any name.

A hook that runs a project file, as husky does with `.husky/pre-commit`, or a
`make lint`, runs what commands wrote there. As with build scripts, review
the changes, or turn hooks off.

For full protection, set git up once:

```sh
git config --global safe.bareRepository explicit
```

and turn hooks off everywhere (yours too), for example in
`~/.config/environment.d/git-safety.conf` and your shell profile. Unlike
`git config --global`, these variables also beat a repository's own
`core.hooksPath`, as husky sets:

```sh
GIT_CONFIG_COUNT=2
GIT_CONFIG_KEY_0=core.hooksPath
GIT_CONFIG_VALUE_0=/dev/null
GIT_CONFIG_KEY_1=core.fsmonitor
GIT_CONFIG_VALUE_1=false
```

## Ubuntu

Since 23.10, Ubuntu lets a program create user namespaces only when an
AppArmor profile allows it, and the sandbox needs them. `kl` detects this
and stops with the commands to run. It never changes the system itself.

### Option A: an AppArmor profile for kl

The profile is attached to a copy of `kl` that only root can change. A
profile for `~/.cargo/bin/kl` would let any program saved there get around
the restriction.

```sh
sudo install -m 755 ~/.cargo/bin/kl /usr/local/bin/kl
/usr/local/bin/kl --apparmor-profile | sudo tee /etc/apparmor.d/kl > /dev/null
sudo apparmor_parser -r /etc/apparmor.d/kl
```

Then start `/usr/local/bin/kl`. After an upgrade, run the `install` command
again. The profile follows Ubuntu's own profile for bubblewrap: `kl` may
create the namespaces of its container, and every program it starts,
sandboxed or not, gets no capabilities. `kl --apparmor-profile` refuses to
print a profile for a binary that users other than root can change.

To remove it:

```sh
sudo apparmor_parser -R /etc/apparmor.d/kl
sudo rm /etc/apparmor.d/kl /usr/local/bin/kl
```

`kl` is linked dynamically, so `LD_PRELOAD` can still run other code under
its profile, as with bubblewrap.

### Option B: turn the restriction off

```sh
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

This lifts the restriction for every program, until the next boot. To keep
it, write the same setting to `/etc/sysctl.d/60-apparmor-namespace.conf`.
