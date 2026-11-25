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
- Bundled skills, such as `mentor`, `study`, `init` and `concise`,
  installed with `kl --install-skills`.
  See [Bundled skills](#bundled-skills).
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

`read`, `write`, `edit`, `ls`, `find`, `grep`, `bash`, `todo`, `task`,
`ask_user`, `web_fetch`, `web_search` and `skill`.

- `read` hands PNG, JPEG, GIF and WebP images (up to 3.5 MB) to the model.
  They are not saved in the session.
- `edit` applies changes atomically and shows a diff with line numbers.
- `edit` and `write` refuse a file that changed since the model last read
  or wrote it, for example one you saved in your editor meanwhile: the
  model has to read it again first, so your change is not lost.
- `bash` streams cleaned output with a timeout and line limits. With
  `--background-commands`, a command such as a dev server can keep running
  across prompts: its output goes to a file, and it is stopped when `kl`
  exits.
- `todo` keeps the plan of a multi-step task.
- `task` hands a part of the work, such as a wide search, to a subagent: a
  conversation of its own with the same model, tools and sandbox, starting
  from the prompt the model writes. Only its answer comes back, so what it
  read along the way does not fill the main conversation. Subagents start
  no subagent, and have no `todo` nor `ask_user`. A task marked
  `read_only` only gets the tools that change nothing (`read`, `ls`,
  `find`, `grep`, `web_fetch`, `web_search`, `skill`); up to 4 of those
  asked together run at the same time. Other tasks run one at a time. The
  tool calls of a subagent show while it works, after the number of its
  call when several run at once, and its tokens count in
  `/status`.
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

## Permissions

Every tool runs without asking unless the `[permissions]` section of the
config file, or the matching options, say otherwise:

```toml
[permissions]
ask = ["bash", "write", "edit"]
deny = ["web_search"]
```

- `deny` (`--deny-tool`): the model does not get the tool.
- `ask` (`--ask-tool`): each call waits for your answer: `Yes`, `Always in
  this session`, which stops asking for that tool until kl exits, or `No`.
  Text typed instead refuses the call and tells the model why. The question
  shows the arguments of the call, and the number of the subagent when one
  asks. Without the terminal UI (`-p` and line mode) there is nobody to
  ask, so these tools are not offered.
- `allow` (`--allow-tool`): the tool runs, which is the default. The option
  is of use to lift, for one run, what the config file says.

A tool in several lists gets the strictest, and an unknown tool name is an
error. The options replace the config file tool by tool. Commands you run
yourself with `!` are never asked for.

This is a convenience, not a boundary: the sandbox is what keeps the model
away from the rest of the system.

## Terminal UI

- Inline terminal UI that keeps native scrollback.
- Live streaming Markdown and reasoning. `Ctrl-O` toggles thinking display.
- Spinner with elapsed time and live `bash` output preview.
- The terminal title shows what kl is doing (`kl: Running bash`, `kl:
  Thinking`, `kl: Question`), and goes back to the old one on exit.
- With `notify = true` in the config file, or `--notify`, kl rings the
  terminal bell when a prompt that ran for 10 seconds or more ends or asks
  a question.
- Persistent input history, model and session pickers, and `/copy` of the last
  reply.
- Messages typed while the agent works join the conversation after the
  running tool calls; one sent as the model answers starts the next turn.
  Cancelling puts the ones left back in the editor.
- `@path` in a prompt attaches the file, read as the `read` tool reads it
  (images included); typing `@` offers the files of the folder.
- Ctrl-V pastes the image of the clipboard into the prompt (PNG, JPEG,
  WebP or GIF, read with `wl-paste` on Wayland or `xclip` on X11): it shows
  as `[Image #1]`, and goes with the prompt while the prompt names it.
  `/retry` sends it again. Images are not saved in the session.
- Shift+Tab switches plan mode: the model only looks around (`read`, `ls`,
  `find`, `grep`, `todo`, `web_fetch`, `web_search`, `skill`, and `task`,
  whose subagents are in plan mode too) and answers with a plan; other
  tools are refused. Switch back and send a message to have the plan carried out.
- `!<command>` runs a shell command as the `bash` tool does, in the sandbox
  when the tools run there, without adding it to the conversation.
- `/output` shows the whole output of the last tool call.
- `/diff` opens a panel on the right: the files the session added,
  changed or deleted, with the lines added and removed, over the diff of
  the one selected (Alt+Up/Down, or Ctrl, to select, PgUp/PgDn to scroll
  it). It follows the changes as the tools run, and works while a prompt
  runs. The files are compared with a snapshot kl takes at the start, and
  again on `/new`: no git command runs, so it works outside git
  repositories too, and leaves out what `.gitignore` lists. The snapshot
  keeps the text of files up to 1 MiB, 64 MiB in all; larger and binary
  files show as changed without a diff. While it is open, kl uses the
  whole screen, and puts the output back in the scrollback once it closes.
- `/<skill> [task]` asks the model to use a skill; typing `/` offers them
  with the commands. See [Using skills](#using-skills).
- `/<command> [args]` sends a prompt template of yours from
  `~/.kluis/commands`. See [Custom commands](#custom-commands).
- Slash commands: `/model`, `/thinking`, `/resume`, `/fork`, `/remember`,
  `/copy`, `/export`, `/output`, `/diff`, `/retry`, `/undo`, `/compact`,
  `/status`, `/new`, `/help`, `/quit`.
- `/undo` puts the files of the project back as they were before the
  last prompt, and drops that prompt and what it led to from the
  conversation. It first lists the files it would restore, create again
  or delete, and asks. Repeating it goes back one prompt more, up to 10.
  See [Undo](#undo).
- `/remember <text>` adds `- <text>` to the `AGENTS.md` at the root of
  the repository (of the current folder outside one), creating it when
  missing, and `/remember --global <text>` to `~/.kluis/AGENTS.md`. The
  instruction also applies to the running conversation. `kl` writes the
  line itself, without the model, and refuses to write through a symlink
  in the project.
- `/fork` goes on in a copy of the session, so you can try another
  direction: the session it copies stays as it was, and can be resumed.
  `kl --continue --fork` and `kl --resume [ID] --fork` do the same at
  start. The session list shows what a session is a fork of. Only the
  conversation is copied, not the files.
- `/retry` sends the last prompt again, dropping what it led to.
- `/export [path]` writes the conversation as Markdown, the tool calls and
  their results included, to `path` or to `kl-<session>.md` in the working
  folder. It refuses to write through a symlink.
- `/thinking` offers only the levels the model takes when its provider says
  which (Anthropic and Ollama), and falls back to `default` when a new model
  does not take the current level. Other providers get every level.
- Falls back to line mode when stdin or stdout is not a terminal.
- For scripts: `-p`/`--print` runs one prompt with standard input appended,
  and `--output-format json` prints one object with `type`, `result`,
  `usage` and `session`, or `error`. kl exits with 1 when a prompt failed,
  line mode included, and 2 on a wrong command line.
- `--output-format stream` prints one JSON object per line as the prompt
  runs, each with a `type`: `text` (once per turn), `tool_call` and
  `tool_result` (paired by `id`), `usage` (per turn, with `subagent: true`
  for the turns of a subagent), `retry`, `truncated`, `compacting`,
  `compacted`, `compact_failed` and `pruned`. The last line
  is the `json` object, with a type of `result` or `error`.
- Both imply `-p`.
- `--output-schema FILE` makes the answer a JSON value matching the JSON
  Schema in `FILE`, and implies `-p` too. The schema is added to the prompt
  and the answer is checked: kl asks once more with what is wrong, then
  fails with exit code 1. As `text` the value is printed alone; `json` and
  the last line of `stream` carry it as `structured` next to `result`.
  The keywords checked are `type`, `enum`, `const`, `properties`,
  `required`, `additionalProperties`, `items`, `minItems`, `maxItems`,
  `minimum`, `maximum`, `minLength`, `maxLength`, `anyOf`, `oneOf` and
  `allOf`. A schema using any other, such as `$ref` or `pattern`, is
  refused, so that no answer passes a rule nobody checked.

## Undo

Before each prompt, `kl` notes how the files of the project differ from
the snapshot `/diff` took at the start. `/undo` compares the files with
that note and puts back every one that changed since: whatever changed
it, the `edit` and `write` tools, a command of the model, or you in your
editor meanwhile. So look at the list before you confirm.

- Like `/diff`, it runs no git command, works outside git repositories,
  and does not look at what `.gitignore` lists.
- Binary files, files over 1 MiB, and what does not fit in the 64 MiB a
  checkpoint may hold cannot be put back: they are listed and left as
  they are. A file made since is deleted whatever it holds.
- Folders left empty are not removed.
- Files are written by `kl` itself, outside the sandbox, never through a
  symbolic link: a link the model left in the project is replaced, not
  followed.
- Checkpoints live in memory, for the last 10 prompts of the session.
  `/new`, `/resume` and a compaction of the conversation forget them.
- `/undo` is for the terminal UI: not with `-p` nor in line mode.

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

### Bundled skills

`kl` comes with the skills in the [`skills`](../skills) folder of this
repository, installed only when you ask:

```sh
kl --install-skills
```

`~/.kluis/skills/.bundled` records the version of each skill installed.
Running `kl --install-skills` again adds the missing skills and updates the
ones you did not change; one you edited is kept, and the new version is
written beside it as `SKILL.md.new`. Once installed, `kl` warns at start
when it comes with newer versions.

They include:

- `mentor`, which questions your plan before agreeing, gives hints instead
  of solutions, and reviews what you wrote. It writes code only when you
  say "show me".
- `study`, which teaches a topic or a codebase one step at a time,
  checking that you understood before moving on.
- `init`, which reads the project and writes its `AGENTS.md`: how to
  build, test and format it, where the code lives and the conventions it
  follows. It updates the file when there is one.
- `concise`, which keeps replies short and plain: the answer first, one
  idea per sentence, no recaps or filler. It saves output tokens and stays
  on until you say "normal mode".

```text
> /init
> /mentor I want to add a cache to the web_fetch tool
> /study how does the sandbox work in this repo?
> /concise
```

A skill's instructions stay in the conversation once loaded. To also keep
the model from changing files, use `mentor` and `study` in plan mode (Shift+Tab).

To bundle another skill, add its folder, holding a `SKILL.md` whose name is
the folder's, to `skills/`: the build picks up every folder there.

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
