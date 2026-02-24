# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project follows Semantic Versioning.

## [Unreleased]

### Added

- Added `aura-orchestrator` crate with a composable supervisor runtime:
  - `submit`, `tick`, `status`, `retry`, and `daemon` flows.
  - Dependency-aware scheduler, optional planner validation, adaptive retry policy, and strict gate orchestration hooks.
  - Provider interfaces for `TaskSource`, `ContextProvider`, `AgentRouter`, `GitHostProvider`, and retry strategy.
- Added `aura-notify-contracts` crate with `NotificationEvent`, `NotificationMessage`, `NotificationSink`, and a no-op sink.
- Added CLI orchestrator commands:
  - `aura orchestrator daemon`
  - `aura orchestrator tick`
  - `aura orchestrator submit`
  - `aura orchestrator status`
  - `aura orchestrator retry`
- Added orchestration domain contracts and store records:
  - Orchestrator task/attempt IDs and statuses.
  - Orchestrator task, dependency, attempt, gate, and event records.
  - Orchestrator store trait APIs.
- Added SQLite orchestrator persistence tables (`orch_tasks`, `orch_task_dependencies`, `orch_attempts`, `orch_gates`, `orch_events`) and in-memory parity support.
- Added SQLite backend implementation in `aura-store` for all core store traits (tasks/workspaces/sessions/executions/board rules/prompt templates) plus orchestrator entities.

### Changed

- Updated project documentation across `README.md`, `CONTRIBUTING.md`, and `CHANGELOG.md`.
- Added a Rust-native `local` executor with local model discovery from Ollama and LM Studio, configurable OpenAI-compatible endpoint support, and interactive model selection when `--model` is omitted.
- Migrated CLI argument parsing to `clap`, added completion generation with `clap_complete`, upgraded local model selection prompt UX with `cliclack`, and switched local discovery subprocesses to `duct`.
- Centralized crate publish metadata/versioning in workspace manifests and integrated `release-plz` for automated release PR + publish flow.

## [0.1.0] - 2026-02-23

### Added

- Initial multi-crate AURA workspace:
  - `aura-contracts`
  - `aura-store`
  - `aura-git`
  - `aura-workspace`
  - `aura-executors`
  - `aura-worker-protocol`
  - `aura-engine`
  - `aura-cli`
- Deterministic orchestration engine with staged execution and follow-up loop support.
- Executor adapters for `codex`, `claude`, `cursor-agent`, `droid`, `amp`, `gemini`, `opencode`, `qwen-code`, `copilot`, and `custom`.
- CLI and TUI flows for running executors and streaming structured logs.
- Usage and cost reporting integration via `fuelcheck-core`.

### Fixed

- CLI behavior and shortcuts around interactive sessions and log rendering.
