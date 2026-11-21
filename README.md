# turnip

**A minimalist, terminal-native coding agent — a Rust port of
[hax](reference/hax/).**

turnip streams markdown and live tool output into your terminal's native
scrollback (it never takes over the screen), runs a multi-round-trip tool
loop with `read`/`edit`/`write`/`bash`, records every conversation as an
append-only JSONL session file, and speaks both the OpenAI-compatible
Chat Completions and Anthropic-compatible Messages wires — against hosted
APIs or local servers (llama.cpp, ollama).

```
crates/
  turnip-core/   the library: providers, agent loop, tools, config, sessions
  turnip-app/    the application: CLI, interactive TUI, one-shot mode
```

This split is the important design decision in the port: **everything
embeddable lives in `turnip-core`** with no global state, no terminal code,
and no tokio runtime — `Config` is an owned value threaded through calls,
tools and hooks are traits, and streaming is callback-based. That is exactly
the surface Python bindings (PyO3/maturin) will wrap; see
[Python bindings](#python-bindings).

## Quick start

```sh
cargo run -p turnip-app            # interactive REPL
cargo run -p turnip-app -- -p "explain this repository"   # one-shot

# connect a provider
export TURNIP_OPENAI_API_KEY=...        # or ANTHROPIC_API_KEY, OPENROUTER_API_KEY, ...
cargo run -p turnip-app -- --provider openai
# local models are first-class:
llama-server -m model.gguf &            # then: turnip --provider llamacpp
ollama serve                            # then: turnip --provider ollama
```

Built-in providers (config overlays and custom endpoints via
`providers.<name>` config blocks): `openai`, `anthropic`, `openrouter`,
`opencode-zen`, `opencode-go`, `deepseek`, `llamacpp`, `ollama`,
`openai-compatible`, `anthropic-compatible`. Autoselection picks the first
available one; `/provider` and `/model` switch live, interactively.

## The interactive frontend

- Streaming markdown rendered line-by-line into native scrollback; only a
  bounded live region (streaming tail, status, input) is redrawn.
- The transcript reads in distinct blocks: submitted prompts are echoed as
  `› …` (accented, like the input prompt) with one blank line between
  turns; tool calls and results commit as `▸ tool(arg)` blocks whose
  result preview uses hax's chrome-dim style — the accent hue drawn
  faint, so it never reads as plain answer text — while reasoning streams
  as dimmed italics and markdown paragraphs keep single blank-line
  breaks whatever the stream chunk boundaries cut them at.
- Ctrl+C interrupts the running turn (cooperatively, through the whole stack:
  stream, retry backoff, and running tool processes). Type-ahead while
  streaming; the submitted line queues for the next turn.
- Slash commands: `/model /provider /effort /new /resume /fork /undo
  /compact /usage /session /config /help /quit`; `@path` mentions expand to
  absolute paths.
- One-shot mode (`-p`) keeps stdout clean (assistant text only; tool noise
  and the resume hint go to stderr) and supports `--json` for JSONL records.

## Configuration

Plain JSON files under the platform directories
(`~/.config/turnip/config.json`, `~/.local/state/turnip/state.json` on
Linux; `%APPDATA%` / `%LOCALAPPDATA%` on Windows). Values resolve in order:

```
run override -> resumed conversation -> environment -> state.json -> config.json -> default
```

with a documented settings registry (`turnip_core::config::SETTINGS`) driving
typed lookups, validation, and the `/config` display. Environment variables
use the `TURNIP_*` prefix (`TURNIP_PROVIDER`, `TURNIP_MODEL`,
`TURNIP_BASH_TIMEOUT`, `TURNIP_OPENAI_BASE_URL`, ...). See the doc comments
in `crates/turnip-core/src/config.rs` for the full list.

Sessions are append-only JSONL files under
`~/.local/state/turnip/sessions/<cwd-bucket>/`: a header, item records,
selection/undo/user-turn control records. `--continue` resumes the newest
one for the working directory; `--resume`/`--fork` pick or branch; `/undo`
reverts user turns (the cut items stay in the file for accounting).

## Architecture notes (ported from hax)

The port keeps hax's fundamental shapes:

- **One canonical conversation model.** A flat log of `Item`s (user /
  assistant / tool_call / tool_result / reasoning / boundary / usage) that
  persistence, rendering, and both wire adapters share; adapters group
  adjacent items when the protocol demands compound messages.
- **Provider-independent stream events.** `StreamEvent` covers text, tool
  call, reasoning (opaque and plaintext), retry, progress, done, error —
  with usage carried on done/error/retry so truncated and retried attempts
  are billed like complete ones.
- **Abort repair semantics.** An interrupted turn keeps only assistant text;
  truncated reasoning is never replayed; the tail is stamped so an empty
  submit continues it well-formed.
- **Compaction** summarizes the history into a seed message (manual
  `/compact`, or automatic before send over a configurable threshold of the
  context window).
- **Append-only sessions with undo records applied by readers.**

Differences are deliberate and documented here rather than hidden:

- **Blocking I/O, no async runtime.** `reqwest::blocking` over a shared
  connection pool; streams are read on background threads and pumped with
  timeouts, which keeps Ctrl+C responsive and the dependency tree small.
  Tooltips: no tokio in the public API, which keeps bindings simple.
- **crossterm instead of a widget toolkit.** hax's TUI is a scrollback
  appender with a live region, not a full-screen widget tree, so the port
  uses crossterm for raw mode/event parsing/SGR and implements the ~300-line
  live-region model directly. Markdown rendering is a compact streaming
  renderer (headings, lists, quotes, fenced code, inline markers), not a
  full CommonMark pipeline.
- **rustls everywhere** (no OpenSSL), so the binary is cross-platform out of
  the box — Linux, macOS, and Windows, where the bash tool prefers a
  `bash` on `PATH` (WSL/Git Bash) and falls back to PowerShell.

### Deferred from the reference

Not yet ported (the seams are ready for them): codex (ChatGPT-subscription
OAuth login) and the OpenAI Responses wire; background bash tasks and the
`task_wait` tool; models.dev catalog prefetch and cost estimation from
published rates (usage totals and provider-reported costs — OpenRouter's,
for example — are shown); image clipboard pasting (the item model, both
wires, and the `read` tool's image ingestion are already in place); preset
stances; transcript/trace files; prompt-cache markers for Anthropic.

## Python bindings

`turnip-core` was shaped for embedding:

- no global mutable state — `Config`, `AgentSession`, `ToolRegistry` are
  owned values;
- all effects flow through three narrow traits: `Provider` (or use the
  built-in registry), `Tool`, and `TurnHooks` for streaming callbacks;
- every persisted type (`Item`, usage, session records) derives
  `Serialize`/`Deserialize`.

A `turnip-py` crate with `pyo3` can therefore wrap `AgentSession`,
`run_user_turn`, the provider registry, and session persistence without
touching terminal or I/O code. A sketch of the intended call path is in
the `turnip-core` crate docs (`lib.rs`).

## Development

```sh
cargo test                     # unit tests across both crates + terminal emulator tests
cargo build -p turnip-app       # binary at target/debug/turnip
python3 scripts/test_tui.py     # real-PTY regression tests (Linux)
python3 scripts/mock_openai_server.py 8765   # mock provider for manual smoke tests
```

The unit tests cover the wire translators (body building and SSE event
translation for both dialects), turn assembly and abort repair, compaction
seam classification, session round-trips with undo, the SSE parser, tool
edge cases (edit uniqueness, bash timeout/interrupt, read slicing, image
sniffing), config tiering and parsers, and the streaming markdown renderer.
The TUI tests cover wrapped-row accounting, scrollback stability, bounded input
and cursor placement, and growing/shrinking live regions. The PTY suite uses
only Python's standard library and a local mock provider to exercise startup
pickers, editing, paste, resize, clearing, and streaming tool turns; it also
checks that idle screens produce no output and exit restores terminal mode.

## License

MIT, matching the reference implementation (`reference/hax/LICENSE`).
