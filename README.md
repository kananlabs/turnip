# turnip

turnip is a terminal coding assistant and a Rust port of [hax](reference/hax/).
It reads and edits files, runs shell commands, and prints replies and tool
output to your terminal's scrollback without taking over the screen.

## Quick start

```sh
export TURNIP_OPENAI_API_KEY=...
cargo run -p turnip-app -- --provider openai
cargo run -p turnip-app -- -p "Explain this repository"
```

Use `--provider ollama` or `--provider llamacpp` with a local server. turnip also
supports Anthropic, OpenRouter, OpenCode Zen and Go, DeepSeek, and custom OpenAI
Chat Completions or Anthropic Messages endpoints.

Type `/help` for commands, `/provider` or `/model` to switch, and Ctrl+C to stop
a running turn. With `-p`, stdout contains only the reply unless you add `--json`
for JSONL output.

## Config and sessions

On Linux, settings live in `~/.config/turnip/config.json` and state lives in
`~/.local/state/turnip/`. See [config.rs](crates/turnip-core/src/config.rs) for
settings and `TURNIP_*` environment variables.

Sessions are saved as JSONL. Use `--continue` to resume the latest session for
this directory, `--resume PATH` to pick one, or `--fork PATH` to start a copy.

## Development

[`turnip-core`](crates/turnip-core/) handles providers, tools, and sessions.
[`turnip-app`](crates/turnip-app/) contains the CLI and terminal UI.

```sh
cargo test
python3 scripts/test_tui.py  # terminal tests on Linux
```

[MIT License](LICENSE)
