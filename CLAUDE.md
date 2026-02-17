# CLAUDE.md -- Thrum

A generic orchestration engine for autonomous AI-driven development, verification,
and release across one or more repositories. Tasks move through a gated pipeline
with configurable quality, proof, and integration checks.

## Build Commands

```bash
# Build all crates
cargo build

# Build release
cargo build --release

# Run tests
cargo test

# Run specific test
cargo test --package thrum-core test_name

# Run CLI
cargo run --bin thrum -- --help

# Run consistency check
cargo run --bin thrum -- check

# Run a single task
cargo run --bin thrum -- run --once
```

## Architecture

Four crates in a workspace:
- **thrum-core**: Domain types (Task state machine, Gate, Repo, Consistency, Traceability, Budget)
- **thrum-db**: Persistence via redb (pure Rust embedded key-value store)
- **thrum-runner**: Subprocess management (Claude CLI, git2, generic processes)
- **thrum-cli**: CLI binary with subcommands (run, task, status, check, release)

## Configuration

All config lives in `configs/`:
- `configs/repos.toml` -- repositories to manage (paths, build/test/lint commands, safety targets)
- `configs/pipeline.toml` -- gates, budget, agent roles, sandbox, subsampling

See `examples/` for ready-to-use configurations:
- `examples/minimal/` -- single-repo setup with only Gate 1 (quality)
- `examples/pulseengine/` -- multi-repo pipeline (meld, loom, synth) with formal verification

To use an example config:
```bash
cp examples/minimal/repos.toml configs/repos.toml
cp examples/minimal/pipeline.toml configs/pipeline.toml
```

## Task State Machine

```
Pending -> Planning -> Planned -> Implementing -> Gate1(Quality) -> Reviewing -> Gate2(Proof)
-> AwaitingApproval -> Approved -> Integrating -> Gate3(Integration) -> Merged
```

Every task goes through a mandatory **Planning** phase where a planner agent produces
a structured mini-spec (file list, verification plan, risk assessment) before
implementation begins. The plan is stored in task metadata and passed to the
implementer as structured context.

Failed gates and rejections cycle back to Implementing (plan is preserved).

## Key Conventions
- All config in `configs/repos.toml` and `configs/pipeline.toml`
- Agent system prompts in `agents/*.md`
- Use `{{CLAUDE_MD}}` placeholder in agent prompts -- replaced with target repo's CLAUDE.md
- Database stored in `thrum.redb` (pure Rust, zero external dependencies)
- Git operations via git2 (libgit2 bindings)

## Error Handling
- `anyhow` in CLI and runner (user-facing errors)
- `thiserror` in core and db (library errors)

## Testing
```bash
cargo test --workspace           # All tests
cargo test --package thrum-db  # DB tests only
```
