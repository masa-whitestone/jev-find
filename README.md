# jev-find (`jfd`)

English | [日本語](README.ja.md)

An AI-native `find`: search your files in natural language. Every decision is made by [Jev](https://docs.typesafe.ai/api).

```
jfd "code that refreshes OAuth tokens" src/
jfd "config files that look broken" -e yaml -e toml -p
jfd "test code" -c unit="unit test" -c e2e="end-to-end test"
jfd "risky code" --rank "security risk" --levels low,medium,high
jfd "meeting notes about the budget" ~/Documents -n 10 -p   # top 10 by probability
```

## Install

Prebuilt binaries for macOS (Apple Silicon / Intel) and Linux (x86_64 / arm64):

```bash
curl -fsSL https://raw.githubusercontent.com/masa-whitestone/jev-find/main/install.sh | sh
```

This installs `~/.local/bin/jfd` (override with `JFD_INSTALL_DIR`). With Rust, you can build from source instead:

```bash
cargo install --git https://github.com/masa-whitestone/jev-find
```

## Setup

You need a TypeSafe API key (get one at [typesafe.ai](https://typesafe.ai)).

```bash
jfd setup
```

It asks for the display language (English / 日本語), the API key (hidden input) and the model, checks the key against the API, and saves everything to `~/.config/jfd/config.json` (mode 0600).
Running `jfd` in a terminal without a key starts this setup automatically.

| Command | What it does |
|---|---|
| `jfd config` | Show current settings (key masked) |
| `jfd config set-key` | Change the API key (hidden input; `-` reads stdin) |
| `jfd config set-lang en\|ja` | Change the display language |
| `jfd config set-model <MODEL>` | Change the default model |
| `jfd config set-base-url <URL>` | Change the API base URL |
| `jfd config set-source <DIR>` | Make `jfd update` rebuild from a source checkout |
| `jfd config unset key\|lang\|model\|base-url\|source\|all` | Reset to default |
| `jfd config test` | Check that the API is reachable |
| `jfd config path` | Print the config file path |

Precedence: **command line > environment (`TYPESAFE_API_KEY` / `JEV_MODEL` / `TYPESAFE_BASE_URL`) > config file > defaults**.
To search for the literal word `setup` or `config`, write `jfd -- config`.

## Update

```bash
jfd update          # update to the latest version
jfd update --check  # only check for a newer release
```

- Installed from a prebuilt binary: downloads the latest GitHub release, verifies its SHA-256, and replaces itself.
- Installed from a source checkout: `git pull`, then `cargo install` (`--no-pull` skips the pull).

## What gets sent

To judge files, jfd sends their path, size and modification age, and when needed the first 4 KB of content (`-H` to change), to the TypeSafe API.
Narrow the search or exclude paths with `-E` where you keep sensitive files.
Files whose content is blocked by the API's firewall are judged by name and metadata only.

## How it works

1. A parallel walker lists files (with `--prune`, Jev decides per directory whether to descend)
2. Stage 1: judge by path, size and age only (cheap, content not read)
3. Stage 2: only undecided files have their first few KB read; classification (`-c`) and scoring (`--rank`) are asked in the same request

Questions are batched across directories: up to 64 files per request in stage 1 (`--batch`) and 16 in stage 2 (`--deep-batch`). Batches that exceed the token limit are split in half and retried automatically.

Answers are cached per file in `~/.cache/jfd/` and invalidated when a file changes, so a run stopped by `--budget N` (request limit) resumes where it left off. `--stats` prints tokens used and an estimated cost.

`--rank` scores range from 0 (lowest level) to 1 (highest level) and show the nearest level name.

### Skipped by default

- `~/Library`, `node_modules`, `__pycache__`, `site-packages`, `DerivedData`, `Pods`
- Bundles such as `.app`, `.framework`, `.photoslibrary`
- Directories containing `CACHEDIR.TAG` (build output such as cargo's `target/`)
- Hidden files and directories (`--hidden` to include)

`--no-ignore` descends everywhere. Directories without permission (macOS protected folders) are skipped silently and only counted (`-v` lists them).

## License

MIT
