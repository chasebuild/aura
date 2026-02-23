# AURA Core Workspace

AURA is a library-first Rust workspace for orchestrating multiple coding agents across local and remote workers.

<img width="2304" height="1536" alt="Group 1228" src="https://github.com/user-attachments/assets/e0f40a49-4e04-4848-816f-5f09003d6551" />

## Crates

- `aura-contracts`: Shared domain models and execution/state machine enums.
- `aura-store`: Storage traits and in-memory implementation.
- `aura-git`: Git and worktree safety operations.
- `aura-workspace`: Multi-repo workspace/worktree orchestration.
- `aura-executors`: Agent executor abstraction and command-backed adapters.
- `aura-worker-protocol`: JSON-RPC protocol/messages for remote worker daemons.
- `aura-engine`: Deterministic state machine, prompt templates, and orchestration runtime.
- `aura-cli`: TUI/CLI runner for launching executors and streaming subprocess logs.

## Architecture

### Execution Model

1. Task execution enters `Queued`.
2. Engine transitions through deterministic stages: `PrepareWorkspace -> Setup -> CodingInitial -> Cleanup -> Review -> Done`.
3. Follow-up loops are supported from `Review -> CodingFollowUp -> Cleanup -> Review`.
4. Failures can transition from any running stage to `Failed`.
5. Cancellation transitions to `Cancelled` with best-effort process interruption.

### Status Model (Hybrid)

- Canonical internal states are enforced by the engine.
- Board columns are customizable and map to canonical states.
- Transition rules validate user-driven moves without weakening engine invariants.

### Remote Worker Model

- JSON-RPC 2.0 over WebSocket.
- Worker heartbeat interval: 5 seconds.
- Offline timeout: 15 seconds.
- Lost workers force running processes into a failed state (`WorkerLost`).

## Testing Scope

- State machine transition coverage.
- Prompt template strict rendering.
- Git safety and worktree recovery behavior.
- Executor command/env behavior and session continuity.
- Worker protocol and lease timeout semantics.
- Cross-crate orchestration scenarios.

## Run The Executor CLI

```bash
cargo run -p aura-cli -- run --executor codex --prompt "Inspect this repository and suggest fixes"
```

Use plain stream mode:

```bash
cargo run -p aura-cli -- run --executor claude --prompt "Review recent changes" --no-tui
```

Run a custom command executor:

```bash
cargo run -p aura-cli -- run --executor custom --prompt "ignored" --base-command sh --param -lc --param 'echo out_line; echo err_line 1>&2' --no-tui
```
