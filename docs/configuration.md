# Configuration

K-Luis reads `~/.kluis/config.toml`. Every setting also has a command-line
option: see `kl --help`.

```toml
[generic]
model = "deepseek-chat"
thinking = "medium"
code_theme = "solarized-dark"
auto_compact_tokens = 100000
searxng_url = "https://searx.example.org"

[sandbox]
read = ["/etc/ssl"]
write = ["~/scratch"]
deny = ["~/work/secrets"]
connect = [11434]
network = false
background_commands = false
max_processes = 1024
tmp_size = "512M"
max_memory = "4G"
env = ["CC", "CFLAGS"]
```

Paths may start with `~`. Sizes take a `K`, `M` or `G` suffix.

Commands see an empty `$HOME` showing only the current folder, the
`read` and `write` paths below it and, read-only, the global skills: list
the toolchains they need, such as `~/.cargo` and `~/.rustup`. See [Home folder](sandbox.md#home-folder).

Lists add up with the matching command-line options. Switches turn on when
either the file or the CLI enables them.

## Settings

| Key                     | What it does                                         | Default             |
| ----------------------- | ---------------------------------------------------- | ------------------- |
| `generic.model`         | Model to use                                         | `deepseek-chat`     |
| `generic.thinking`      | Thinking effort: `default`, `off`, `low`, `medium`, `high`, `xhigh`, `max` | `default`           |
| `generic.code_theme`          | Colors of code blocks, see [Code theme](#code-theme)            | `base16-ocean-dark` |
| `generic.auto_compact_tokens` | Auto-compact context at this many prompt tokens (`0` disables) | `100000`            |
| `generic.searxng_url`   | SearXNG instance for `web_search`, with its JSON format on | DuckDuckGo          |
| `sandbox.read`          | Extra folders the model can read, also in `$HOME`    | none                |
| `sandbox.write`         | Extra folders the model can change, also in `$HOME`  | none                |
| `sandbox.deny`          | Paths to hide, on top of the built-in credentials    | none                |
| `sandbox.connect`       | TCP ports the model can connect to                   | none                |
| `sandbox.network`       | Allow any network                                    | `false`             |
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

## Command-line options

| Option                | What it does                                       |
| --------------------- | -------------------------------------------------- |
| `-c`, `--continue`    | Continue the latest session in this folder         |
| `--resume [ID]`       | Resume a session, with a picker when no ID is given |
| `--no-session`        | Do not read or write session files                 |
| `--strict`            | Make the current folder read-only                  |
| `--no-sandbox`        | Run tools on the host                              |
| `--login`, `--logout` | Log in to or out of a subscription (`anthropic`)   |

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
