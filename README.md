# agyline

A fast, zero-dependency, Cometix-style native Rust status line for [Antigravity CLI](https://antigravity.google/docs/cli/statusline) (`agy`).

## Features

- **Zero external crates (`std` only)**: compiles offline in seconds into a ~500 KB single binary with sub-10 ms startup and no script-runtime overhead.
- **Cometix-style visual layout**:
  - **Left**: active model (with per-family Nerd Font icons for Gemini, Claude, OpenAI/Codex, DeepSeek, Copilot) + reasoning effort, Vim mode, tool confirmation indicator, workspace directory, Git branch/status/ahead-behind, context usage, prompt cache hit ratio.
  - **Right**: `cycle_mode` (`plan` / `auto` / `accept-edits`), live activity (running subagents, background tasks, artifacts), model-aware `5H` / `7D` rolling quota usage & reset countdown, estimated session cost.
- **Zero-block Git SWR cache**: reads `.git/HEAD` directly on the render path and refreshes `git status` asynchronously in a detached background worker (using `CreateProcessW` with `bInheritHandles = FALSE` on Windows to avoid pipe hangs).
- **4-step responsive layout**: gracefully shrinks right segments, then left segments, and wraps cleanly to two lines on narrow terminals.
- **Windows Terminal tab title sync**: updates the shared console title to the active `conversation_title` via `SetConsoleTitleW` without polluting `stdout`.
- **Windows orphan self-reaper**: periodically cleans up `0ms`-CPU `Suspended` orphaned copies of itself left behind when `cmd /c` is terminated mid-spawn, protected by 6 strict Win32 guards.

## Requirements

- Rust toolchain (`cargo` / `rustc`, edition 2021)
- A [Nerd Font](https://www.nerdfonts.com/) (v3.5+ recommended for native `nf-cod-claude` and `nf-cod-openai` glyphs), or pass `--classic` for plain Unicode fallback.

## Build & Install

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

## CLI Flags

| Flag | Description |
| :--- | :--- |
| `--classic`, `--no-nerdfont` | Use plain Unicode symbols instead of Nerd Font glyphs |
| `--dir-style name\|parent\|tilde` | Directory display style (default: `name`) |
| `--quota-mode used\|remaining` | Show quota as used % (default) or remaining % |
| `--fast-git` | Read `.git/HEAD` only and skip background `git status` |
| `--no-title` | Disable syncing `conversation_title` to the Windows Terminal tab title |
| `--no-model`, `--no-dir`, `--no-git`, `--no-context`, `--no-cache` | Hide individual left segments |
| `--no-mode`, `--no-vim`, `--no-activity`, `--no-quota`, `--no-cost` | Hide individual right segments |
| `--preview [--width N]` | Render sample payloads at wide, medium, and narrow widths |
| `--legend` | Print segment legend and available flags |
| `--version` | Print version string |

Set `NO_COLOR=1` to disable ANSI color output.

## Acknowledgements

- Visual design inspired by [Haleclipse/CCometixLine](https://github.com/Haleclipse/CCometixLine).
- Antigravity CLI payload conventions informed by [weby-homelab/antigravity-cli-statusline](https://github.com/weby-homelab/antigravity-cli-statusline) and the official Antigravity CLI documentation.

## License

[MIT](LICENSE)
