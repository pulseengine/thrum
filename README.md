<div align="center">

# Thrum

<sup>Gate-based pipeline orchestrator for AI-driven development</sup>

&nbsp;

![Rust](https://img.shields.io/badge/Rust-CE422B?style=flat-square&logo=rust&logoColor=white&labelColor=1a1b27)
![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue?style=flat-square&labelColor=1a1b27)

</div>

&nbsp;

A gate-based pipeline orchestrator for autonomous AI-driven development across one or more repositories.

> [!NOTE]
> Part of the PulseEngine toolchain. Orchestrates AI-assisted development pipelines across PulseEngine repositories.

Thrum manages a task queue where coding agents (Claude Code, OpenCode, Aider, or any CLI/API agent) implement changes that must pass through configurable verification gates before merging. Failed gates trigger automatic retries with memory of previous failures. Human approval is required between the proof gate and integration.

## What It Does

- Runs AI coding agents against a task backlog, one task at a time per repository (parallel across repos)
- Enforces a 3-gate verification pipeline: Quality (fmt/clippy/test), Proof (Z3/Rocq), Integration (cross-repo)
- Tracks tasks through a 9-state machine with retry logic and human approval checkpoints
- Stores agent memory (error patterns, successful approaches) with semantic decay for context injection
- Supports multiple repositories with per-repo concurrency limits
- Provides a REST API for task management and status monitoring

## Architecture

Five crates in a Rust workspace:

| Crate | Purpose |
|-------|---------|
| `thrum-core` | Domain types: Task state machine, gates, memory, roles, subsample config |
| `thrum-db` | Persistence via redb (embedded key-value store, no external database) |
| `thrum-runner` | Agent subprocess management, backend abstraction, sandbox isolation |
| `thrum-api` | REST API server (axum) for task CRUD and status |
| `thrum-cli` | CLI binary: `thrum run`, `thrum task`, `thrum status`, `thrum check` |

### Task State Machine

```
Pending -> Claimed -> Implementing -> Gate1(Quality) -> Reviewing -> Gate2(Proof)
  -> AwaitingApproval -> Approved -> Integrating -> Gate3(Integration) -> Merged
```

Failed gates cycle back to Implementing with retry count incremented (max 3). Rejected tasks return to Implementing with human feedback.

### Verification Gates

| Gate | Checks | Configurable |
|------|--------|-------------|
| Gate 1 (Quality) | `cargo fmt --check`, `cargo clippy`, `cargo test` | Yes, per-repo in `repos.toml` |
| Gate 2 (Proof) | Z3 SMT solver, Rocq/Coq proofs | Yes, optional per-repo |
| Gate 3 (Integration) | Cross-repo pipeline (configurable step sequence) | Yes, in `pipeline.toml` |

### Backend Swappability

Coding agents are configured in `pipeline.toml`, not hardcoded:

```toml
[[backends]]
name = "claude-code"
type = "agent"
command = "claude"
prompt_args = ["-p", "{prompt}", "--output-format", "json"]

[[backends]]
name = "aider"
type = "agent"
command = "aider"
prompt_args = ["--message", "{prompt}", "--yes"]
```

Any CLI tool that accepts a prompt and produces code changes can be used as a backend.

## Quick Start

```bash
# Build
cargo build --release

# Configure repositories
cp examples/minimal/repos.toml configs/repos.toml
cp examples/minimal/pipeline.toml configs/pipeline.toml
# Edit configs/repos.toml to point at your repository

# Add a task
cargo run --bin thrum -- task add --repo my-project \
  --title "Implement feature X" \
  --description "Add support for X with tests"

# Run the pipeline (single task)
cargo run --bin thrum -- run --once

# Run with parallel agents (one per repo)
cargo run --bin thrum -- run --agents 4
```

## Configuration

All configuration lives in `configs/`:

- `configs/repos.toml` — repositories, build/test/lint commands, safety classification
- `configs/pipeline.toml` — gates, backends, roles, sandbox, budget, subsampling

See `examples/` for ready-to-use configurations:
- `examples/minimal/` — single-repo with Gate 1 only
- `examples/pulseengine/` — multi-repo pipeline with formal verification

## Development

```bash
cargo test --workspace           # All tests
cargo bench --workspace          # All benchmarks
cargo clippy --workspace --tests -- -D warnings
cargo +nightly udeps --workspace # Unused dependency check
```

## License

Apache-2.0

---

<div align="center">

<sub>Part of <a href="https://github.com/pulseengine">PulseEngine</a> &mdash; formally verified WebAssembly toolchain for safety-critical systems</sub>

</div>
