# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project follows Semantic Versioning.

## [Unreleased]

### Changed

- Updated project documentation across `README.md`, `CONTRIBUTING.md`, and `CHANGELOG.md`.
- Added a Rust-native `local` executor with local model discovery from Ollama and LM Studio, configurable OpenAI-compatible endpoint support, and interactive model selection when `--model` is omitted.

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
