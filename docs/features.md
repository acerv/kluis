# Features

## Agent

- Streaming replies and reasoning, with configurable thinking effort.
- Rate limits, overloaded servers and dropped connections are retried up to
  five times with exponential backoff (1 s to 16 s), honoring `Retry-After`.
- Read-only tools (`read`, `ls`, `find`, `grep`) run in parallel.
- `AGENTS.md` instructions are loaded from `~/.kluis/AGENTS.md`, the
  repository root, and the current folder. Project ones must not be symlinks,
  which could point at files the sandbox hides.
- Skills: folders with a `SKILL.md` in `~/.kluis/skills`. Only each
  skill's name and description are in the system prompt; the model loads
  the rest with the `skill` tool when a task needs it. See [Skills](#skills).
- Live model discovery across configured providers, with a per-provider
  timeout.
- Sessions are persisted as JSONL per workspace, appended to as they go and
  rewritten without the replaced messages on compaction, so long ones stay
  small enough to resume. Continue the latest or resume any session by ID.
- Automatic conversation compaction when prompt tokens reach the configured
  threshold (default 100K), plus manual `/compact [focus]`.
- When the provider refuses a request as larger than the context window, the
  outputs of old tool calls are dropped from the conversation and the turn is
  asked again.

## Tools

`read`, `write`, `edit`, `ls`, `find`, `grep`, `bash`, `todo`, `ask_user`,
`web_fetch`, `web_search` and `skill`.

- `read` hands PNG, JPEG, GIF and WebP images (up to 3.5 MB) to the model.
  They are not saved in the session.
- `edit` applies changes atomically and shows a diff with line numbers.
- `bash` streams cleaned output with a timeout and line limits. With
  `--background-commands`, a command such as a dev server can keep running
  across prompts: its output goes to a file, and it is stopped when `kl`
  exits.
- `todo` keeps the plan of a multi-step task.
- `ask_user` asks you a question mid-task, with suggested answers to pick from
  or your own text (interactive mode only).
- `web_fetch` reads a web page as Markdown (or raw with `raw: true`), 24 KB at
  a time. It runs outside the sandbox, so it is offered only with `--network`
  (or `network = true`) or `--no-sandbox`, where it reaches public addresses
  only, or when hosts are listed with `--web-host` (or `web_hosts`), which it
  is then limited to: see [Web tools](sandbox.md#web-tools).
- `web_search` returns the titles, URLs and snippets of up to 10 results. It
  asks DuckDuckGo, or the SearXNG instance set with `--searxng-url` (or
  `searxng_url`), and is offered when `web_fetch` is.
- `skill` returns the `SKILL.md` of a skill and its folder, up to 64 KB. It
  is offered when there are skills.
- Tool results are bounded JSON, with `{"error": "..."}` responses so the
  model recovers instead of derailing.

## Terminal UI

- Inline terminal UI that keeps native scrollback.
- Live streaming Markdown and reasoning. `Ctrl-O` toggles thinking display.
- Spinner with elapsed time and live `bash` output preview.
- Persistent input history, model and session pickers, and `/copy` of the last
  reply.
- Messages typed while the agent works join the conversation after the
  running tool calls; one sent as the model answers starts the next turn.
  Cancelling puts the ones left back in the editor.
- `@path` in a prompt attaches the file, read as the `read` tool reads it
  (images included); typing `@` offers the files of the folder.
- Shift+Tab switches plan mode: the model only looks around (`read`, `ls`,
  `find`, `grep`, `todo`, `web_fetch`, `web_search`, `skill`) and answers
  with a plan; other tools are refused. Switch back and send a message to have the plan carried out.
- `!<command>` runs a shell command as the `bash` tool does, in the sandbox
  when the tools run there, without adding it to the conversation.
- `/output` shows the whole output of the last tool call.
- `/<skill> [task]` asks the model to use a skill; typing `/` offers them
  with the commands. See [Using skills](#using-skills).
- `/<command> [args]` sends a prompt template of yours from
  `~/.kluis/commands`. See [Custom commands](#custom-commands).
- Slash commands: `/model`, `/thinking`, `/resume`, `/copy`, `/output`,
  `/retry`, `/compact`, `/status`, `/new`, `/help`, `/quit`.
- `/retry` sends the last prompt again, dropping what it led to.
- `/thinking` offers only the levels the model takes when its provider says
  which (Anthropic and Ollama), and falls back to `default` when a new model
  does not take the current level. Other providers get every level.
- Falls back to line mode when stdin or stdout is not a terminal.
- For scripts: `-p`/`--print` runs one prompt with standard input appended,
  and `--json` prints one object with `result`, `usage` and `session`, or
  `error`. kl exits with 1 when a prompt failed, line mode included, and 2
  on a wrong command line.

## Skills

A skill is a folder holding a `SKILL.md`, and any scripts or documents it
needs. Its front matter names and describes it:

```markdown
---
name: release-notes
description: Write release notes from the git log. Use when asked for a changelog.
---

Run `scripts/collect.sh <tag>` and group the commits by crate...
```

The name is up to 64 lowercase letters, digits and hyphens. Skills come
only from `~/.kluis/skills`, which you control: projects cannot bring their
own. Up to 100 skill folders are read; `kl` warns at start about the skills
it cannot use.

The `skill` tool reads a `SKILL.md` on the host. `~/.kluis/skills` is also
shown read-only in the sandbox, so the model can read the other files of a
skill and run its scripts there; the rest of `~/.kluis` stays hidden.
When a folder shown to commands holds `~/.kluis`, such as `$HOME`, all of it
stays hidden and its skills are left out, with a warning. Neither
`~/.kluis/skills`, a skill folder nor its `SKILL.md` may be a symlink, so
only what you put there is used.

### Using skills

Install a skill by putting its folder in `~/.kluis/skills`, then start
`kl`. The banner counts the skills it found:

```sh
mkdir -p ~/.kluis/skills
cp -r release-notes ~/.kluis/skills/
kl
```

The model loads a skill with the `skill` tool when a request matches its
description:

```text
> write the release notes for v0.2
```

Or ask for one by name, with the task after it:

```text
> /release-notes since v0.1, only user-visible changes
```

With `-p`, `kl -p "/release-notes since v0.1"` does the same.

## Custom commands

A custom command is a prompt you send often, kept in
`~/.kluis/commands/<name>.md`. `/<name> [args]` sends its text, with each
`$ARGUMENTS` replaced by the arguments; without `$ARGUMENTS`, the arguments
are appended after a blank line. An optional front matter describes it for
the completion menu, else its first line does:

```markdown
---
description: Review a change for bugs
---

Review $ARGUMENTS for bugs and missing tests. Do not edit any file.
```

```text
> /review the last commit
```

The prompt is expanded before it is sent, so the conversation holds the
full text. `kl -p "/review the last commit"` and line mode do the same, and
`git diff | kl -p /review` reviews the diff on standard input.

The name is up to 64 lowercase letters, digits and hyphens. Commands come
only from `~/.kluis/commands`, which you control: projects cannot bring their
own, and neither the folder nor a command file may be a symlink. Up to 100
files of at most 32 KiB are read at start; `kl` warns about the ones it
cannot use. A command of `kl`, such as `/model`, wins over a custom command
of the same name, and a custom command wins over a skill; `kl` warns about
both. The folder stays hidden from the sandbox, with the rest of
`~/.kluis`.
