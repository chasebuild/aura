# Contributing to AURA

## Prerequisites

- Rust stable (workspace `rust-version = 1.85`)
- Toolchain components: `rustfmt`, `clippy`

## Local Setup

```bash
git clone <repo-url>
cd aura
cargo build --workspace
```

## Development Workflow

1. Create a branch from `main`.
2. Implement a single logical change.
3. Run formatting, linting, and tests.
4. Open a pull request with a clear summary and validation notes.

Branch naming convention:

- `feat/<short-description>`
- `fix/<short-description>`
- `chore/<short-description>`
- `docs/<short-description>`

## Commit Style

Use Conventional Commits where possible:

- `feat(cli): add ...`
- `fix(engine): handle ...`
- `docs(readme): update ...`

Keep each commit focused on one logical change.

## Quality Checks

Before opening a PR, run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If behavior changes, add or update tests in the relevant crate.

## Version Bumps and Release

Use `release-plz` for version bumps and crate publication.
The npm scripts are thin wrappers around `release-plz`.

Install once:

```bash
cargo install release-plz
```

Update versions/changelog locally:

```bash
npm run changelog -- --dry-run
```

Open or refresh release PR:

```bash
npm run release:pr -- --dry-run
```

Publish crates:

```bash
npm run release:publish -- --dry-run
```

## Project Layout

- `/crates/contracts`: shared domain and state models
- `/crates/store`: storage backends
- `/crates/git`: git/worktree safety primitives
- `/crates/workspace`: multi-repo workspace coordination
- `/crates/executors`: executor adapters
- `/crates/worker-protocol`: remote worker protocol
- `/crates/engine`: orchestration engine
- `/crates/cli`: `aura` command-line and TUI interface

## Pull Request Checklist

- [ ] Change is scoped and reviewable
- [ ] Code is formatted
- [ ] Lint passes with no new warnings
- [ ] Tests pass locally
- [ ] Documentation updated (`README.md`, `CHANGELOG.md`, or crate docs when needed)
