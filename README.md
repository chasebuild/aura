# AURA Core Workspace

AURA is a library-first Rust workspace for orchestrating coding-agent execution across local and remote workers.

<img width="2304" height="1536" alt="Group 1228" src="https://github.com/user-attachments/assets/e0f40a49-4e04-4848-816f-5f09003d6551" />

## Requirements

- Rust `stable` (`rust-version = 1.85`)
- `cargo`, `clippy`, and `rustfmt` (via `rust-toolchain.toml`)

## Quick Start

```bash
cargo build --workspace
cargo test --workspace
```

Run the CLI with the default executor (`codex`):

```bash
cargo run -p aura-cli -- run --executor codex --prompt "Inspect this repository and suggest fixes"
```

Run in plain streaming mode (no TUI):

```bash
cargo run -p aura-cli -- run --executor claude --prompt "Review recent changes" --no-tui
```

Run the custom executor:

```bash
cargo run -p aura-cli -- run --executor custom --prompt "ignored" --base-command sh --param -lc --param 'echo out_line; echo err_line 1>&2' --no-tui
```

## CLI Usage

```text
Usage:
  aura run --executor <name> --prompt <text> [options]
  aura tui --executor <name> --prompt <text> [options]
  aura help
```

Executors:

| Executor       | Description                                                                                                    |
| -------------- | -------------------------------------------------------------------------------------------------------------- |
| `codex`        | OpenAI Codex CLI adapter with JSON streaming output, plus optional model and sandbox/approval controls.        |
| `claude`       | Anthropic Claude CLI adapter using stream-json input/output for structured orchestration.                      |
| `cursor-agent` | Cursor Agent CLI adapter with stream-json output and optional `--force` / `--trust` behavior.                  |
| `droid`        | Droid CLI adapter configured for JSON output with optional model, auto-permission level, and reasoning effort. |
| `amp`          | Sourcegraph Amp adapter (`npx`) in execute + stream-json mode, with optional broad tool permission mode.       |
| `gemini`       | Google Gemini CLI adapter (`npx`) with ACP mode, optional model selection, and optional yolo tooling flag.     |
| `opencode`     | OpenCode AI adapter (`npx`) with configurable model/variant/agent mode and host/port/env overrides.            |
| `local`        | Rust-native local adapter for OpenAI-compatible local endpoints, with model discovery from Ollama and LM Studio. |
| `qwen-code`    | Qwen Code adapter (`npx`) in ACP mode with optional yolo execution behavior.                                   |
| `copilot`      | GitHub Copilot CLI adapter (`npx`) with tool allow/deny controls and optional model selection.                 |
| `custom`       | Generic command-backed executor where you provide the base command and params directly.                        |

Key options:

- `--cwd <path>`
- `--session <id>`
- `--review`
- `--var KEY=VALUE` (repeatable)
- `--base-command <cmd>`
- `--param <arg>` (repeatable)
- `--append-prompt <text>`
- `--model <name>`
- `--openai-endpoint <url>` (for `local` executor)
- `--yolo`
- `--force` / `-f`
- `--trust`
- `--auto-approve <true|false>`
- `--allow-all-tools`
- `--no-tui`

For `--executor local`, if `--model` is omitted, the CLI prompts you to select from discovered local models (default is the first discovered model).

## Workspace Crates

- `aura-contracts`: Shared domain models and execution/state machine enums.
- `aura-store`: Storage traits with in-memory and SQLite modules.
- `aura-git`: Git and worktree safety operations.
- `aura-workspace`: Multi-repo workspace/worktree orchestration.
- `aura-executors`: Executor abstraction and adapter implementations.
- `aura-worker-protocol`: JSON-RPC protocol/messages for remote worker daemons.
- `aura-engine`: Deterministic state machine, prompts, and orchestration runtime.
- `aura-cli`: TUI/CLI runner for launching executors and streaming logs.

## Engine Model

Execution flow:

1. Task execution enters `Queued`.
2. Engine transitions through deterministic stages: `PrepareWorkspace -> Setup -> CodingInitial -> Cleanup -> Review -> Done`.
3. Follow-up loops are supported from `Review -> CodingFollowUp -> Cleanup -> Review`.
4. Failures can transition from running stages to `Failed`.
5. Cancellation transitions to `Cancelled` with best-effort interruption.

Status model:

- Canonical internal states are enforced by the engine.
- Board columns are customizable and map to canonical states.
- Transition rules validate user-driven moves without weakening engine invariants.

Remote worker model:

- JSON-RPC 2.0 over WebSocket.
- Worker heartbeat interval: 5 seconds.
- Offline timeout: 15 seconds.
- Lost workers force running processes into `WorkerLost`.

## Development Commands

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
