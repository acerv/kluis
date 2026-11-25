# Sandbox

Model commands run in a [`hakoniwa`](https://github.com/souk4711/hakoniwa)
container that shows only the folders you configure.

## What is isolated

- The current folder is writable. With `--strict` it is read-only, and
  `--no-sandbox` turns the container off.
- `.git` stays read-only, and a new `.git` is disabled after each prompt.
- There is no network by default. You can allow all of it, some TCP ports
  (Linux 6.7+), which are open on every host, not only locally, or some
  hosts through a proxy `kl` runs. See [Allowed hosts](#allowed-hosts).
- `$HOME` is an empty private folder that lives in memory and is gone when
  `kl` exits. Only the current folder, the `sandbox.read` and
  `sandbox.write` paths below it and, read-only, `~/.kluis/skills` show
  there. See [Home folder](#home-folder).
- Cloud drives, network shares and credentials in `$HOME` are hidden, even
  inside a folder you show: `~/.ssh`, `~/.aws`, `gh`, registry and AI tool
  tokens, browser profiles, shell history and more. `sandbox.deny` hides
  additional paths. A folder you show that is itself on such a filesystem,
  or the current folder, stays: `kl` warns at startup instead.
- API keys and `~/.kluis` are never passed into the sandbox, but for
  `~/.kluis/skills`, read-only.
- The kernel attack surface is reduced: no user namespaces, only common socket
  types, no mount changes, no core dumps.
- `kl` refuses to run as root or with sudo, even with `--no-sandbox`: root
  keeps its privileges in the container and could undo it.
- Processes and `/tmp` are limited. Leftover programs are stopped after each
  prompt, unless you allow [background commands](#background-commands). On
  low memory commands are killed first.

See [Configuration](configuration.md) for the settings that control this.

To check the result, `kl --sandbox-info` prints the sandbox a run with the
same options and config file would get, then exits: the paths commands can
change, the ones they can only read, the ones hidden from them, the
network, the limits, the environment variables passed, the hosts of the
proxy and the web tools offered, followed by the warnings a run would start with. The paths are the
mounts of the container itself, in the order they are made.

```sh
kl --sandbox-info
kl --sandbox-info --strict --allow-read ~/.cargo
```

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

Three tools run outside the sandbox because they need the user or the
network: `ask_user`, `web_fetch` and `web_search`. `web_fetch` and
`web_search` are offered with `--network` (or `network = true`) or
`--no-sandbox`, and `web_fetch` also when hosts are listed for it. See
[Web tools](#web-tools).

Commands allowed the network with `--network` reach any host, LAN and
localhost included: the limits of the web tools do not apply to them.

Running a file of unknown format makes the kernel run `modprobe` for a
`binfmt-XXXX` module, outside the sandbox. Only a host setting can stop it:
`kernel.modprobe` or `kernel.modules_disabled`.

## Web tools

`web_fetch` and `web_search` connect only to public addresses. Loopback,
private, link-local (cloud metadata endpoints such as `169.254.169.254`),
shared, multicast and reserved addresses are refused, also when written
inside an IPv6 address. A name is judged by the addresses it resolves to,
the connection uses only those, and each redirect is checked again.
Proxies from the environment are not used, as they would resolve names
themselves.

With `--network` this matters in plan mode only, as commands can then
reach your machine and LAN themselves.

`--web-host`, or `web_hosts` in `[sandbox]`, lets `web_fetch` reach only the
hosts listed and their subdomains: `docs.rs` also allows `www.docs.rs`. A
listed host may be private, such as `localhost`. Listing hosts offers
`web_fetch` even without `--network`, so the model can read the
documentation it needs while commands stay off the network, and cannot
send what it reads to a host you did not list. `web_search` is not offered
then. It asks only its search engine, which may be private when set with
`--searxng-url`.

## Allowed hosts

`--allow-host`, or `hosts` in `[sandbox]`, lets commands reach the hosts
listed and their subdomains, and nothing else, without `--network`. It
fits builds that download packages:

```toml
[sandbox]
hosts = ["crates.io", "static.crates.io", "index.crates.io"]
```

Commands keep a network of their own with nothing in it. `kl` brings up
its loopback interface and listens there, on `127.0.0.1:3128`, with a
proxy running on your machine: it forwards plain HTTP requests and tunnels
`CONNECT` ones, as HTTPS uses, to the listed hosts only. `http_proxy`,
`https_proxy` and `all_proxy`, also in capitals, point at it, and
`no_proxy` keeps `localhost` off it. Programs that ignore these variables
get no network, and DNS does not work: the proxy resolves names itself.

A refused request gets a `403` answer naming the host, and the result of
the command lists the hosts refused, so you know which one to add. As for
`web_hosts`, a listed host may be private.

The proxy limits where commands connect, not what they send there: it
reads no TLS, and a command may still upload the project to a host you
listed, such as a code hosting site. List only hosts you trust with it.

It needs a sandbox without network, so it cannot be used with
`--allow-connect`, and limits nothing with `--network`, where commands
reach every host: `kl` warns then.

## Background commands

`--background-commands`, or `background_commands = true` in `[sandbox]`, lets
the model start commands that keep running across prompts, such as a dev
server or a watcher. They stay in the sandbox with its limits and are stopped
when `kl` exits.

They are off by default because nothing else runs in the sandbox between
prompts. A background command can create a git repository at any time:
`kl` disables new ones after every prompt while such commands run, and once
more when they stop, but your git could find one in between. Set git up as
below before turning them on.

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
