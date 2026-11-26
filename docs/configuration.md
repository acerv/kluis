# Configuration

K-Luis reads `~/.kluis/config.toml`. Every setting also has a command-line
option: see [Command-line options](#command-line-options).

```toml
[generic]
model = "deepseek-chat"
thinking = "medium"
theme = "dark"
code_theme = "solarized-dark"
auto_compact_tokens = 100000
searxng_url = "https://searx.example.org"
notify = true

[sandbox]
read = ["/etc/ssl"]
write = ["~/scratch"]
deny = ["~/work/secrets"]
connect = [11434]
network = false
web_hosts = ["docs.rs", "github.com"]
background_commands = false
max_processes = 1024
tmp_size = "512M"
max_memory = "4G"
env = ["CC", "CFLAGS"]

[permissions]
allow = ["read", "ls", "find", "grep"]
ask = ["bash", "write", "edit"]
deny = ["web_search"]
```

Paths may start with `~`. Sizes take a `K`, `M` or `G` suffix.

Commands see an empty `$HOME` showing only the current folder, the
`read` and `write` paths below it and, read-only, `~/.kluis/skills`: list
the toolchains they need, such as `~/.cargo` and `~/.rustup`. See
[Home folder](sandbox.md#home-folder).

Lists add up with the matching command-line options. Switches turn on when
either the file or the CLI enables them.

`[permissions]` names tools: see [Permissions](features.md#permissions).
`--allow-tool`, `--ask-tool` and `--deny-tool` replace what the file says,
tool by tool.

## Settings

| Key                     | What it does                                         | Default             |
| ----------------------- | ---------------------------------------------------- | ------------------- |
| `generic.model`         | Model to use                                         | `deepseek-chat`     |
| `generic.thinking`      | Thinking effort: `default`, `off`, `low`, `medium`, `high`, `xhigh`, `max` | `default`           |
| `generic.theme`               | Colors for a `dark` or `light` terminal                          | `dark`              |
| `generic.code_theme`          | Colors of code blocks, see [Code theme](#code-theme)            | `tokyo-night`, `inspired-github` with the light theme |
| `generic.auto_compact_tokens` | Auto-compact context at this many prompt tokens (`0` disables) | `100000`            |
| `generic.searxng_url`   | SearXNG instance for `web_search`, with its JSON format on | DuckDuckGo          |
| `generic.notify`        | Ring the terminal bell when a prompt that ran for 10 seconds or more ends or asks a question | `false` |
| `sandbox.read`          | Extra folders the model can read, also in `$HOME`    | none                |
| `sandbox.write`         | Extra folders the model can change, also in `$HOME`  | none                |
| `sandbox.deny`          | Paths to hide, on top of the built-in credentials    | none                |
| `sandbox.connect`       | TCP ports the model can connect to                   | none                |
| `sandbox.network`       | Allow any network                                    | `false`             |
| `sandbox.hosts`         | Hosts commands reach through a proxy, with their subdomains, without `network`; not with `connect`, see [Allowed hosts](sandbox.md#allowed-hosts) | none |
| `sandbox.web_hosts`     | Only hosts `web_fetch` reaches, with their subdomains and private addresses; offers it without `network`, see [Web tools](sandbox.md#web-tools) | any public host |
| `sandbox.background_commands` | Let the model keep commands running across prompts, see [Background commands](sandbox.md#background-commands) | `false` |
| `sandbox.max_processes` | Processes and threads at once                        | `1024`              |
| `sandbox.tmp_size`      | Size of the private `/tmp`                           | a quarter of RAM    |
| `sandbox.max_memory`    | Virtual memory limit per command                     | none                |
| `sandbox.env`           | Environment variables passed to commands             | none                |

## Code theme

Code blocks that name a known language, like ` ```sh ` or ` ```rust `, are
syntax-highlighted. `code_theme` picks the colors: one of the built-in themes,
or the path of a TextMate theme file (`.tmTheme`), as many editor themes come.

| Theme                  | Background |
| ---------------------- | ---------- |
| `tokyo-night`          | dark       |
| `base16-ocean-dark`    | dark       |
| `base16-eighties-dark` | dark       |
| `base16-mocha-dark`    | dark       |
| `solarized-dark`       | dark       |
| `base16-ocean-light`   | light      |
| `inspired-github`      | light      |
| `solarized-light`      | light      |

```toml
[generic]
code_theme = "~/.kluis/dracula.tmTheme"
```

A value with a `/` is a file. Code blocks without a known language, and
inline code, keep a plain style. With `NO_COLOR`, all of it stays plain.
Without `code_theme`, the light theme uses `inspired-github`.

## Command-line options

Options with a config key take the same values as the key.

| Option                         | Config key                    | What it does |
| ------------------------------ | ----------------------------- | ------------ |
| `--model NAME`                 | `generic.model`               | Model to use; its name picks the provider |
| `--thinking LEVEL`             | `generic.thinking`            | Thinking effort; a resumed session keeps its own unless this is given |
| `--theme dark\|light`          | `generic.theme`               | Colors for a dark or light terminal |
| `--code-theme NAME\|PATH`      | `generic.code_theme`          | Colors of code blocks, see [Code theme](#code-theme) |
| `--auto-compact-tokens N`      | `generic.auto_compact_tokens` | Auto-compact the conversation at this many prompt tokens (`0` disables) |
| `--notify`                     | `generic.notify`              | Ring the terminal bell when a prompt that ran for 10 seconds or more ends or asks a question |
| `-c`, `--continue`             |                               | Continue the latest session in this folder |
| `-r`, `--resume [ID]`          |                               | Resume a session, with a picker when no ID is given |
| `--fork`                       |                               | With `--continue` or `--resume`, go on in a copy of the session and leave the session itself as it is |
| `--no-session`                 |                               | Do not read or write session files |
| `--no-sandbox`                 |                               | Run tools on the host |
| `--strict`                     |                               | Make the current folder read-only |
| `--allow-read PATH`            | `sandbox.read`                | Also allow reading this path; `$HOME` is empty but for the allowed paths and the current folder |
| `--allow-write PATH`           | `sandbox.write`               | Also allow changing this path |
| `--deny-read PATH`             | `sandbox.deny`                | Hide this path from the harness and its commands |
| `--allow-connect PORT`         | `sandbox.connect`             | Let the harness connect to this TCP port, e.g. for a local model |
| `--network`                    | `sandbox.network`             | Allow any network, to commands too |
| `--allow-host HOST`            | `sandbox.hosts`               | Let commands reach this host and its subdomains through a proxy, without `--network`, see [Allowed hosts](sandbox.md#allowed-hosts) |
| `--web-host HOST`              | `sandbox.web_hosts`           | Let `web_fetch` reach only this host and its subdomains, private addresses included, even without `--network`, see [Web tools](sandbox.md#web-tools) |
| `--searxng-url URL`            | `generic.searxng_url`         | Search the web with this SearXNG instance instead of DuckDuckGo |
| `--max-processes N`            | `sandbox.max_processes`       | Processes and threads the harness and its commands may run at once |
| `--tmp-size SIZE`              | `sandbox.tmp_size`            | Size of the private `/tmp` |
| `--max-memory SIZE`            | `sandbox.max_memory`          | Virtual memory limit for commands |
| `--background-commands`        | `sandbox.background_commands` | Let commands, such as a server, keep running across prompts, see [Background commands](sandbox.md#background-commands) |
| `--pass-env NAME`              | `sandbox.env`                 | Also pass this environment variable to commands |
| `--allow-tool TOOL`            | `permissions.allow`           | Let this tool run without asking, see [Permissions](features.md#permissions) |
| `--ask-tool TOOL`              | `permissions.ask`             | Ask before each call of this tool; without the terminal UI, the tool is not offered |
| `--deny-tool TOOL`             | `permissions.deny`            | Do not offer this tool to the model |
| `--config PATH`                |                               | Config file to read |
| `-p`, `--print`                |                               | Run the prompt once and exit, with standard input appended when it is not a terminal |
| `--output-format FORMAT`       |                               | `text`, `json` or `stream`, see [features](features.md); `json` and `stream` imply `--print` |
| `--output-schema FILE`         |                               | Make the answer a JSON value matching the JSON Schema in this file; implies `--print` |
| `--login PROVIDER`, `--logout PROVIDER` |                      | Log in to or out of a subscription (`anthropic`), then exit |
| `--apparmor-profile`           |                               | Print an AppArmor profile that lets kl create its sandbox on Ubuntu, then exit, see [sandbox](sandbox.md) |
| `--install-skills`             |                               | Install the bundled skills in `~/.kluis/skills`, or update the ones you did not change, then exit |
| `--sandbox-info`               |                               | Print what the sandbox lets commands see and reach with these options and the config file, then exit |

## Environment variables

| Variable           | What it does                        |
| ------------------ | ----------------------------------- |
| `HARNESS_MODEL`    | Model to use, like `--model`        |
| `HARNESS_THINKING`            | Thinking effort, like `--thinking`                              |
| `HARNESS_AUTO_COMPACT_TOKENS` | Auto-compact threshold, like `--auto-compact-tokens`            |
| `HARNESS_CONFIG`              | Config file to read, like `--config`                            |
| `NO_COLOR`         | Turn colors off                     |

## API keys

Provider credentials go in `~/.kluis/auth.toml`, keyed by the lowercase
provider name. Each entry may set `api_key` and/or `endpoint`:

```toml
[deepseek]
api_key = "sk-..."

[anthropic]
api_key = "sk-ant-..."

[openai]
api_key = "sk-..."

# Custom endpoint, for example a local OpenAI-compatible server.
[genai_1]
endpoint = "http://localhost:8000/v1"
```

Keep the file private: `chmod 600 ~/.kluis/auth.toml`. Environment variables
such as `DEEPSEEK_API_KEY` still take precedence, and an OAuth login takes
precedence over `api_key`.

To use a Claude Pro/Max subscription instead of a key, run
`kl --login anthropic`.
