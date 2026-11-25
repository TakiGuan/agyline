# agyline

A fast, zero-dependency, Cometix-style native Rust status line for [Antigravity CLI](https://antigravity.google/docs/cli/statusline) (`agy`).

![agyline preview](assets/preview.png)

## Features

- **Zero external crates (`std` only)**: compiles offline in seconds into a ~520 KB single binary with sub-10 ms startup, built-in JSON & TOML parsers, and no script-runtime overhead.
- **Cometix-style visual layout**:
  - **Left**: active model (with per-family Nerd Font icons for Gemini, Claude, OpenAI/Codex, DeepSeek, Copilot) + reasoning effort, Vim mode, tool confirmation indicator, workspace directory, Git branch/status/ahead-behind, context usage, prompt cache hit ratio.
  - **Right**: `cycle_mode` (`plan` / `auto` / `accept-edits`), live activity (running subagents, background tasks, artifacts), model-aware `5H` / `7D` rolling quota usage & reset countdown, estimated session cost.
- **Zero-block Git SWR cache**: reads `.git/HEAD` directly on the render path and refreshes `git status` asynchronously in a detached background worker (using `CreateProcessW` with `bInheritHandles = FALSE` on Windows to avoid pipe hangs).
- **4-step responsive layout**: gracefully shrinks right segments, then left segments, and wraps cleanly to two lines on narrow terminals.
- **Windows Terminal tab title sync**: updates the shared console title to the active `conversation_title` via `SetConsoleTitleW` without polluting `stdout`.
- **Windows orphan self-reaper & exit watcher**: runs at most one global zero-CPU background watcher (`WaitForMultipleObjects` on live `agy.exe` handles) that automatically reaps `0ms`-CPU `Suspended` orphaned copies (including hot-swap `.old.*` backups) whenever an `agy` session exits, then exits cleanly once the last `agy` closes.

## Requirements

- Rust toolchain (`cargo` / `rustc`, edition 2021)
- A [Nerd Font](https://www.nerdfonts.com/) (v3.5+ recommended for native `nf-cod-claude` and `nf-cod-openai` glyphs), or pass `--classic` for plain Unicode fallback.

## Download Prebuilt Binaries

Prebuilt binaries for Windows (`x86_64`), Linux (`x86_64`), and macOS (`arm64` / `x86_64`) are published on the [GitHub Releases](https://github.com/TakiGuan/agyline/releases) page along with `SHA256SUMS.txt`.

## Build from Source

```bash
git clone https://github.com/TakiGuan/agyline.git
cd agyline
cargo build --release
```

Copy the compiled binary (`target/release/agyline.exe` on Windows or `target/release/agyline` on macOS/Linux) to a permanent location, for example `~/.antigravity/agyline.exe`.

## Antigravity CLI Configuration

Add `statusLine` to `~/.gemini/antigravity-cli/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "C:\\Users\\<you>\\.antigravity\\agyline.exe"
  }
}
```

## Configuration File (Optional)

`agyline` works out of the box with zero configuration. If you want to customize watcher timing, Git cache TTL, or visible segments without a long CLI command, generate a TOML config next to the binary:

```bash
agyline --init-config
```

On startup, `agyline` looks beside its own executable for `<exe_stem>.toml` first (for example `statusline.toml` when renamed to `statusline.exe`), then `agyline.toml`. Precedence is **CLI flags > TOML config > built-in defaults**.

```toml
glyphs = "nerd"          # "nerd" | "classic"
title = true             # Sync conversation_title to Windows Terminal tab

[layout]
width = "auto"           # "auto" or integer column count
dir_style = "name"       # "name" | "parent" | "tilde"
quota_mode = "used"      # "used" | "remaining"

[git]
fast = false             # true = read .git/HEAD only (skip git status)
cache_ms = 5000          # SWR fresh cache TTL in ms
timeout_ms = 10000       # Background git status timeout in ms

[watcher]
enabled = true           # Windows single-instance agy.exe exit watcher
poll_ms = 30000          # Rescan interval for newly opened agy.exe (1000..300000)
exit_settle_ms = 500     # Wait after agy exit before reaping (100..5000)
reap_interval_secs = 60  # Periodic orphan reap interval (10..3600)
reap_min_age_secs = 15   # Min age for periodic orphan reap (1..3600)

[segments]
model = true
dir = true
git = true
context = true
cache = true
mode = true
vim = true
subagents = true
tasks = true
artifacts = true
quota = true
cost = true
```

## CLI Flags

| Flag | Description |
| :--- | :--- |
| `--classic`, `--no-nerdfont` | Use plain Unicode symbols instead of Nerd Font glyphs |
| `--dir-style name\|parent\|tilde` | Directory display style (default: `name`) |
| `--quota-mode used\|remaining` | Show quota as used % (default) or remaining % |
| `--fast-git` | Read `.git/HEAD` only and skip background `git status` |
| `--no-title` | Disable syncing `conversation_title` to the Windows Terminal tab title |
| `--no-watch` | Disable the single-instance Windows `agy.exe` exit watcher |
| `--no-model`, `--no-dir`, `--no-git`, `--no-context`, `--no-cache` | Hide individual left segments |
| `--no-mode`, `--no-vim`, `--no-activity`, `--no-quota`, `--no-cost` | Hide individual right segments |
| `--init-config` | Write a commented default `.toml` config file beside the binary |
| `--preview [--width N]` | Render sample payloads at wide, medium, and narrow widths |
| `--legend` | Print segment legend and available flags |
| `--version` | Print version string |

Set `NO_COLOR=1` to disable ANSI color output.

## Acknowledgements

- Visual design inspired by [Haleclipse/CCometixLine](https://github.com/Haleclipse/CCometixLine).
- Antigravity CLI payload conventions informed by [weby-homelab/antigravity-cli-statusline](https://github.com/weby-homelab/antigravity-cli-statusline) and the official Antigravity CLI documentation.
- Inspired by discussions and community sharing on [LINUX DO](https://linux.do).

## License

[MIT](LICENSE)
