# Thrum

A gate-based pipeline orchestrator for autonomous AI-driven development across one or more repositories.

Thrum manages a task queue where coding agents (Claude Code, OpenCode, Aider, or any CLI/API agent) implement changes that must pass through configurable verification gates before merging. Failed gates trigger automatic retries with memory of previous failures. Human approval is required between the proof gate and integration.

## What It Does

- Runs AI coding agents against a task backlog, one task at a time per repository (parallel across repos)
- Enforces a 3-gate verification pipeline: Quality (fmt/clippy/test), Proof (Z3/Rocq), Integration (cross-repo)
- Tracks tasks through a 9-state machine: Pending, Claimed, Implementing, Gate1, Reviewing, Gate2, AwaitingApproval, Approved, Integrating, Gate3, Merged
- Stores agent memory (error patterns, successful approaches) with semantic decay for context injection
- Supports multiple repositories with per-repo concurrency limits
- Provides a REST API for task management and status monitoring

## What It Does Not Do

- Does not replace your CI/CD pipeline. It orchestrates the pre-merge development loop.
- Does not train or fine-tune models. It calls external coding agents as subprocesses or API clients.
- Does not handle deployment, monitoring, or incident response.
- Does not provide a GUI. It is a CLI tool with an optional JSON API.
- Does not guarantee correctness. Gates reduce risk but are only as strong as the checks configured.

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
name = "opencode"
type = "agent"
command = "opencode"
prompt_args = ["--prompt", "{prompt}"]

[[backends]]
name = "aider"
type = "agent"
command = "aider"
prompt_args = ["--message", "{prompt}", "--yes"]
```

Any CLI tool that accepts a prompt and produces code changes can be used as a backend. Chat-based API backends (Anthropic, OpenAI-compatible) are also supported.

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

- `configs/repos.toml` -- repositories, build/test/lint commands, safety classification
- `configs/pipeline.toml` -- gates, backends, roles, sandbox, budget, subsampling

See `examples/` for ready-to-use configurations:
- `examples/minimal/` -- single-repo with Gate 1 only
- `examples/pulseengine/` -- multi-repo pipeline with formal verification

## Quality

| Check | Status |
|-------|--------|
| Rust edition | 2024 |
| `cargo clippy -D warnings` | 0 warnings |
| `cargo test --workspace` | 87 tests |
| `cargo +nightly miri test --package thrum-core` | 40 tests, 0 UB |
| proptest (property-based) | 9 tests |
| loom (concurrency model) | 2 tests |
| criterion benchmarks | 11 benchmarks |
| `cargo +nightly udeps` | 0 unused deps |

Pre-commit hook: `cp scripts/pre-commit .git/hooks/pre-commit`

## Comparison with Other Tools

| | Thrum | VS Code Multi-Agent (v1.109) | OpenCode | Aider | SWE-agent | OpenHands | Copilot Workspace |
|---|---|---|---|---|---|---|---|
| **Architecture** | Gate pipeline + state machine | Conductor-delegate in IDE | Client/server + ACP | CLI pair programmer | Agent-Computer Interface | Agent platform + sandbox | Cloud task environment |
| **Verification gates** | 3 formal gates (quality/proof/integration) | No | No | No | No | No | Tests pre-PR (no gate pipeline) |
| **Task state machine** | 9 states with transitions | No | No | No | No | No | No |
| **Multi-repo** | Native, parallel per-repo | Partial (workspace) | No | No | No | Partial | Yes |
| **Backend swappable** | Yes (any CLI agent or chat API) | Yes (Copilot providers) | Yes (AI SDK) | Yes (any LLM) | Yes (any LLM) | Yes | No (Copilot only) |
| **Sandbox isolation** | Docker / OS-native / none | No | Docker | No | Docker | Docker | Cloud managed |
| **Parallel agents** | Yes (semaphore-bounded) | Yes (subagents) | No | No | No | No | No |
| **Human-in-the-loop** | Required at Gate 2 approval | Optional | No | Interactive | No | Optional | Optional |
| **License** | Apache-2.0 | Proprietary | Open source | MIT | MIT | MIT | Proprietary |

### Where Thrum Differs

**vs. VS Code Multi-Agent**: VS Code delegates subtasks within a single coding session. Thrum orchestrates a full development lifecycle (implement, verify, review, integrate) across separate tasks and repositories. VS Code has no verification gates or persistent task state.

**vs. OpenCode / Aider**: These are interactive coding assistants for a single repository. Thrum is a batch orchestrator that queues tasks, runs agents unattended, and enforces quality gates before merge. They can be used *as backends* within Thrum.

**vs. SWE-agent / OpenHands**: These focus on single-issue resolution, typically for benchmarks. Thrum manages an ongoing task backlog with retry logic, cross-repo integration testing, and human approval checkpoints.

**vs. Copilot Workspace**: Cloud-hosted, closed-source, locked to GitHub's model. Thrum is self-hosted, backend-agnostic, and enforces a formal gate pipeline that Copilot Workspace does not.

### Where Others Are Stronger

- **VS Code Multi-Agent**: Tighter IDE integration, real-time feedback, richer UI
- **Aider**: Better interactive pair-programming experience, repository map for context
- **SWE-agent**: Purpose-built for benchmark evaluation (SWE-bench)
- **Copilot Workspace**: Zero setup, cloud-managed, integrated with GitHub PRs
- **OpenHands**: Web UI, broader tool ecosystem, larger community

## Development

```bash
cargo test --workspace           # All tests
cargo bench --workspace          # All benchmarks
cargo clippy --workspace --tests -- -D warnings
cargo +nightly udeps --workspace # Unused dependency check
MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test --package thrum-core -- --skip proptests
```

## License

Apache-2.0
