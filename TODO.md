# TODO

## Sandbox

- [x] Hide FUSE and network mounts below shown folders (cloud drives, network shares)
- [x] Block Unix sockets without `--network`: they lead to host programs that reach the network
- [x] Refuse user namespaces, `clone3` and io_uring to commands (kernel attack surface)
- [x] Allow only the socket families commands need (`AF_INET`, `AF_INET6`, `AF_NETLINK`, `AF_UNIX` with `--network`): `AF_VSOCK` is not isolated by the network namespace and reaches host and hypervisor services, and MCTP, PPPoX, SMC, KCM and `AF_ALG` are open too
- [x] Allow only common protocols (TCP, UDP, ping, netlink routing): for others the kernel loads a module as root, such as SCTP or L2TP
- [x] No core dumps: a crash handed a core file shaped by commands to the host's handler, such as systemd-coredump running as root
- [ ] Running a file of unknown format makes the kernel run `modprobe` for a `binfmt-XXXX` module; seccomp cannot see it, only a host setting (`kernel.modprobe`, `modules_disabled`) can stop it
- [ ] `/proc/interrupts` and `/proc/pressure` show host activity to commands (interrupt counts can tell when the user types); hakoniwa mounts `/proc` last, over any mask, and commands have no capabilities: masking needs a user namespace of the worker's own
- [x] Give the container its own IPC and UTS namespaces: commands share SysV memory with host programs, and their segments outlive the sandbox
- [x] Call `protect_self()` in the tool worker: commands can read its environment and write forged results into its pipe to the host
- [x] Never restart the tool worker from a replaced program: in a writable folder (a build of kl in its own project), a command could rename another program over it, which would run without the seccomp filter
- [x] Protect `.git` of new `git init`: a model-made nested repo with `core.fsmonitor` runs commands when the user (or their shell prompt) runs git there
- [x] Stop programs commands leave running after each prompt: one started with `setsid` outlived its command and could recreate a `.git` right after the check; an init in the container also reaps them
- [x] Warn when git would use repositories not named `.git` (`safe.bareRepository`): git takes any folder holding `HEAD`, `objects` and `refs` for one
- [x] Fail closed when a writable folder cannot be read: commands could `chmod 300` a folder to hide a `.git` from the scan, so the next session left it writable
- [ ] Review mode: commands write to a copy of the project, and kl applies the changes after each prompt without `.git`, editor settings and other dangerous files (as AgentFS, Brood Box, yoloAI)
- [ ] Editor settings the model writes (`.zed/`, `.vscode/`, `.idea/`) can run commands when the project is opened; like build files, only review mode would cover them
- [x] Refuse a symlinked `AGENTS.md`: the host follows it at startup, pulling hidden files (`~/.kluis/auth.toml`, `~/.ssh`) into the prompt
- [x] Hide credentials readable in `$HOME` (`gh`, `npm`, `cargo`, `docker`, `kube`, `aws`, browser and mail profiles, chat apps, keyrings, shell history): a built-in list, always hidden whatever the config says
- [ ] Hide `$HOME` by default and show only toolchains, for credentials of tools the built-in list misses
- [x] `--allow-connect`: refuse UDP sockets, which Landlock does not limit
- [x] `--allow-connect`: document that Landlock allows the port on every host, not only locally
- [ ] Warn at startup when a shown folder (the project, `$HOME`) is itself on a FUSE or network filesystem: file access there reaches the network
- [x] Line mode prints model text raw: strip control characters so escape sequences (e.g. OSC 52 clipboard) cannot reach the terminal, also from notices and errors naming paths commands chose
- [ ] Refuse more unneeded system calls (`keyctl`, `bpf`, `perf_event_open`, `userfaultfd`, mount family, `kexec`, module loading) for kernels with laxer defaults
- [x] Default limits for memory and the private `/tmp`: `/tmp` holds a quarter of the memory, and the kernel kills commands first when memory runs out
- [ ] Limit the memory of all commands together with a cgroup (`memory.max`), where systemd delegates the memory controller to the user (not everywhere)

## Compatibility

- [ ] Ubuntu 23.10+: AppArmor blocks unprivileged user namespaces; at least fail with a clear message and document the setup

## Tools

- [ ] `read`: images as attachments
- [x] `ask_user` tool: pause and ask the user a question (with options) mid-task, then feed the answer back as tool output
- [x] `web_fetch`: let the model read documentation and issues (decide how it fits the sandbox: network is off by default)
- [x] Todo tool so the model can keep a plan outside the context
- [ ] Plan mode: the model proposes a plan and the user approves it before any changes
- [ ] Subagents for large tasks
- [ ] Permissions system: per-tool `allow`/`ask`/`deny` in `config.toml`, so `--no-sandbox` is not unguarded
- [ ] File checkpoints of `edit`/`write` changes (git snapshot or per-turn backup) to make them reversible

## Agent & Session

- [ ] Auto-compaction when `input_tokens` approaches the model's context limit
- [ ] Recover when the provider rejects an oversized request (compact and retry)
- [x] Retry with exponential backoff on transient API errors (rate limit, 5xx, dropped stream), honoring `Retry-After`
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

- [x] README: install, sandbox requirements, config, login
