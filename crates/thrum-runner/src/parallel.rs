//! Parallel agent execution engine.
//!
//! Dispatches multiple agents concurrently with:
//! - Global semaphore capping total concurrent agents
//! - Per-repo semaphore (1 per repo) preventing git working directory conflicts
//! - Atomic task claiming via redb single-writer transactions
//! - Graceful shutdown via CancellationToken

use crate::backend::BackendRegistry;
use crate::event_bus::EventBus;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thrum_core::agent::{AgentId, AgentSession};
use thrum_core::budget::BudgetTracker;
use thrum_core::event::EventKind;
use thrum_core::repo::ReposConfig;
use thrum_core::task::{RepoName, Task};
use thrum_db::task_store::{ClaimCategory, TaskStore};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Configuration for the parallel engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Maximum concurrent agents across all repos.
    pub max_agents: usize,
    /// Maximum concurrent agents per repo (default: 1).
    pub per_repo_limit: usize,
    /// Per-session budget in USD (passed through to pipeline).
    pub session_budget_usd: Option<f64>,
    /// How often to poll for new tasks when idle.
    pub poll_interval: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_agents: 4,
            per_repo_limit: 1,
            session_budget_usd: None,
            poll_interval: Duration::from_secs(5),
        }
    }
}

/// Shared context passed to each spawned agent.
///
/// Everything behind `Arc` so it can be cheaply cloned into spawned tasks.
pub struct PipelineContext {
    pub db: Arc<redb::Database>,
    pub repos_config: Arc<ReposConfig>,
    pub agents_dir: PathBuf,
    pub registry: Arc<BackendRegistry>,
    pub session_budget_usd: Option<f64>,
    /// Shared budget tracker for global spending enforcement.
    /// Protected by a mutex for thread-safe concurrent access.
    pub budget: Arc<Mutex<BudgetTracker>>,
    pub roles: Option<Arc<thrum_core::role::RolesConfig>>,
    pub sandbox_config: Option<crate::sandbox::SandboxConfig>,
    /// Event bus for real-time pipeline observability.
    pub event_bus: EventBus,
    /// Integration gate steps (empty = Gate 3 passes vacuously).
    pub integration_steps: Vec<thrum_core::gate::IntegrationStep>,
}

/// Result of a single agent run.
struct AgentResult {
    session: AgentSession,
    outcome: Result<()>,
}

/// Run the parallel dispatch loop.
///
/// Returns when:
/// - All categories exhausted and no agents in flight
/// - Shutdown token is cancelled (graceful drain)
pub async fn run_parallel(
    ctx: Arc<PipelineContext>,
    config: EngineConfig,
    repo_filter: Option<RepoName>,
    shutdown: CancellationToken,
) -> Result<()> {
    let global_sem = Arc::new(Semaphore::new(config.max_agents));

    // Per-repo semaphores: keyed by repo name string
    let mut repo_sems: HashMap<String, Arc<Semaphore>> = HashMap::new();
    for repo in &ctx.repos_config.repo {
        repo_sems.insert(
            repo.name.to_string(),
            Arc::new(Semaphore::new(config.per_repo_limit)),
        );
    }
    let repo_sems = Arc::new(repo_sems);

    let mut join_set: JoinSet<AgentResult> = JoinSet::new();

    tracing::info!(
        max_agents = config.max_agents,
        per_repo = config.per_repo_limit,
        "parallel engine started"
    );

    ctx.event_bus.emit(EventKind::EngineLog {
        level: thrum_core::event::LogLevel::Info,
        message: format!(
            "parallel engine started (max_agents={}, per_repo={})",
            config.max_agents, config.per_repo_limit
        ),
    });

    loop {
        if shutdown.is_cancelled() {
            tracing::info!("shutdown requested, stopping dispatch");
            break;
        }

        // Reap completed agents
        while let Some(result) = join_set.try_join_next() {
            reap_agent_result(result, &ctx.event_bus);
        }

        // Dispatch batch: try to claim and spawn agents
        let dispatched = dispatch_batch(
            &ctx,
            &config,
            repo_filter.as_ref(),
            &global_sem,
            &repo_sems,
            &mut join_set,
        )
        .await?;

        if dispatched == 0 && join_set.is_empty() {
            tracing::info!("no tasks to dispatch and no agents in flight, exiting");
            break;
        }

        if dispatched == 0 {
            // Nothing new to dispatch; wait for an agent to finish or poll interval
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("shutdown during wait");
                    break;
                }
                Some(result) = join_set.join_next() => {
                    reap_agent_result(result, &ctx.event_bus);
                }
                _ = tokio::time::sleep(config.poll_interval) => {}
            }
        }
    }

    // Graceful drain: wait for all in-flight agents
    if !join_set.is_empty() {
        tracing::info!(
            count = join_set.len(),
            "waiting for in-flight agents to complete"
        );
        while let Some(result) = join_set.join_next().await {
            reap_agent_result(result, &ctx.event_bus);
        }
    }

    tracing::info!("parallel engine stopped");
    ctx.event_bus.emit(EventKind::EngineLog {
        level: thrum_core::event::LogLevel::Info,
        message: "parallel engine stopped".into(),
    });
    Ok(())
}

/// Log and emit events for a completed agent result.
fn reap_agent_result(result: Result<AgentResult, tokio::task::JoinError>, event_bus: &EventBus) {
    match result {
        Ok(agent_result) => {
            let elapsed = agent_result.session.elapsed_secs();
            let success = agent_result.outcome.is_ok();
            match &agent_result.outcome {
                Ok(()) => tracing::info!(
                    agent = %agent_result.session.agent_id,
                    task = %agent_result.session.task_id,
                    elapsed_secs = elapsed,
                    "agent completed successfully"
                ),
                Err(e) => tracing::error!(
                    agent = %agent_result.session.agent_id,
                    task = %agent_result.session.task_id,
                    error = %e,
                    "agent failed"
                ),
            }
            event_bus.emit(EventKind::AgentFinished {
                agent_id: agent_result.session.agent_id,
                task_id: agent_result.session.task_id,
                success,
                elapsed_secs: elapsed,
            });
        }
        Err(e) => {
            tracing::error!(error = %e, "agent task panicked");
        }
    }
}

/// Try to dispatch agents for each claim category in priority order.
///
/// Returns the number of agents spawned this batch.
async fn dispatch_batch(
    ctx: &Arc<PipelineContext>,
    _config: &EngineConfig,
    repo_filter: Option<&RepoName>,
    global_sem: &Arc<Semaphore>,
    repo_sems: &Arc<HashMap<String, Arc<Semaphore>>>,
    join_set: &mut JoinSet<AgentResult>,
) -> Result<usize> {
    let categories = [
        ClaimCategory::RetryableFailed,
        ClaimCategory::Approved,
        ClaimCategory::Pending,
    ];

    let mut dispatched = 0;

    for &category in &categories {
        loop {
            // Check global capacity
            let global_permit = match global_sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => break, // At capacity
            };

            // Try to claim a task
            let task_store = TaskStore::new(&ctx.db);
            let claimed = task_store.claim_next("pre-dispatch", category, repo_filter)?;

            let task = match claimed {
                Some(t) => t,
                None => {
                    // Return the permit — nothing to claim in this category
                    drop(global_permit);
                    break;
                }
            };

            // Check per-repo capacity
            let repo_key = task.repo.to_string();
            let repo_sem = repo_sems
                .get(&repo_key)
                .context(format!("no semaphore for repo {repo_key}"))?;

            let repo_permit = match repo_sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    // Repo at capacity — unclaim the task back to its original state
                    unclaim_task(&ctx.db, &task, category)?;
                    drop(global_permit);
                    break;
                }
            };

            // Generate agent ID and spawn
            let agent_id = AgentId::generate(&task.repo, &task.id);
            let repo_config = ctx
                .repos_config
                .get(&task.repo)
                .context(format!("no config for repo {}", task.repo))?;
            let work_dir = repo_config.path.clone();

            // Update the claimed status with the actual agent ID
            let mut claimed_task = task.clone();
            claimed_task.status = thrum_core::task::TaskStatus::Claimed {
                agent_id: agent_id.to_string(),
                claimed_at: chrono::Utc::now(),
            };
            claimed_task.updated_at = chrono::Utc::now();
            task_store.update(&claimed_task)?;

            let session = AgentSession::new(
                agent_id.clone(),
                task.id.clone(),
                task.repo.clone(),
                work_dir,
            );

            tracing::info!(
                agent = %agent_id,
                task = %task.id,
                repo = %task.repo,
                category = ?category,
                "dispatching agent"
            );

            ctx.event_bus.emit(EventKind::AgentStarted {
                agent_id: agent_id.clone(),
                task_id: task.id.clone(),
                repo: task.repo.clone(),
            });

            let ctx = Arc::clone(ctx);
            let category_copy = category;

            join_set.spawn(async move {
                let mut session = session;
                let outcome = run_agent_task(&ctx, task, category_copy).await;
                session.finish();
                // Permits are dropped when this future completes, releasing semaphores
                drop(global_permit);
                drop(repo_permit);
                AgentResult { session, outcome }
            });

            dispatched += 1;
        }
    }

    Ok(dispatched)
}

/// Unclaim a task back to its pre-claimed state when dispatch can't proceed.
fn unclaim_task(db: &redb::Database, task: &Task, category: ClaimCategory) -> Result<()> {
    let task_store = TaskStore::new(db);
    let mut t = task_store
        .get(&task.id)?
        .context("task disappeared during unclaim")?;

    // Restore to the appropriate pre-claim status
    t.status = match category {
        ClaimCategory::Pending => thrum_core::task::TaskStatus::Pending,
        ClaimCategory::Approved => thrum_core::task::TaskStatus::Approved,
        ClaimCategory::RetryableFailed => {
            // We can't perfectly restore the old report, so leave it as Pending
            // for the retry path to pick up again
            thrum_core::task::TaskStatus::Pending
        }
    };
    t.updated_at = chrono::Utc::now();
    task_store.update(&t)?;
    Ok(())
}

/// Execute the pipeline for a single claimed task.
///
/// This is the per-agent entrypoint, called from within a spawned tokio task.
/// It delegates to the appropriate pipeline function based on claim category.
async fn run_agent_task(ctx: &PipelineContext, task: Task, category: ClaimCategory) -> Result<()> {
    let task_store = TaskStore::new(&ctx.db);
    let gate_store = thrum_db::gate_store::GateStore::new(&ctx.db);

    let roles_ref = ctx.roles.as_deref();

    match category {
        ClaimCategory::RetryableFailed => {
            crate::parallel::pipeline::retry_task_pipeline(
                &task_store,
                &gate_store,
                &ctx.repos_config,
                &ctx.agents_dir,
                &ctx.registry,
                &ctx.event_bus,
                &ctx.budget,
                task,
            )
            .await
        }
        ClaimCategory::Approved => {
            crate::parallel::pipeline::post_approval_pipeline(
                &task_store,
                &gate_store,
                &ctx.repos_config,
                &ctx.event_bus,
                &ctx.integration_steps,
                task,
            )
            .await
        }
        ClaimCategory::Pending => {
            crate::parallel::pipeline::run_task_pipeline(
                &task_store,
                &gate_store,
                &ctx.repos_config,
                &ctx.agents_dir,
                &ctx.registry,
                roles_ref,
                &ctx.event_bus,
                &ctx.budget,
                task,
            )
            .await
        }
    }
}

/// Pipeline functions extracted for sharing between sequential and parallel paths.
pub mod pipeline {
    use crate::backend::{AiBackend, AiRequest, AiResponse, BackendRegistry};
    use crate::claude::load_agent_prompt;
    use crate::event_bus::EventBus;
    use crate::git::GitRepo;
    use anyhow::{Context, Result};
    use chrono::Utc;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use thrum_core::budget::{self, BudgetEntry, BudgetTracker, SessionType};
    use thrum_core::event::EventKind;
    use thrum_core::gate::{run_gate, run_integration_gate_configured};
    use thrum_core::repo::ReposConfig;
    use thrum_core::task::{CheckpointSummary, GateLevel, MAX_RETRIES, Task, TaskStatus};
    use thrum_db::gate_store::GateStore;
    use thrum_db::task_store::TaskStore;
    use tokio::sync::Mutex;

    /// Emit a task state change event.
    fn emit_state_change(event_bus: &EventBus, task: &Task, from: &str, to: &str) {
        event_bus.emit(EventKind::TaskStateChange {
            task_id: task.id.clone(),
            repo: task.repo.clone(),
            from: from.to_string(),
            to: to.to_string(),
        });
    }

    /// Record the cost of an AI invocation into the shared budget tracker.
    ///
    /// Estimates cost from token counts in the response, falling back to
    /// the role's per-invocation budget if tokens are unavailable.
    async fn record_invocation_cost(
        budget: &Arc<Mutex<BudgetTracker>>,
        task_id: i64,
        session_type: SessionType,
        response: &AiResponse,
        role_budget_usd: f64,
    ) {
        let estimated_cost = budget::estimate_cost(
            &response.model,
            response.input_tokens,
            response.output_tokens,
            role_budget_usd,
        );

        let entry = BudgetEntry {
            task_id,
            session_type,
            model: response.model.clone(),
            input_tokens: response.input_tokens.unwrap_or(0),
            output_tokens: response.output_tokens.unwrap_or(0),
            estimated_cost_usd: estimated_cost,
            timestamp: Utc::now(),
        };

        let mut tracker = budget.lock().await;
        tracker.record(entry);

        tracing::info!(
            estimated_cost_usd = estimated_cost,
            remaining_usd = tracker.remaining(),
            ceiling_usd = tracker.ceiling_usd,
            "recorded invocation cost"
        );
    }

    /// Full pipeline: Pending/Claimed → Implement → Gate1 → Review → Gate2 → AwaitingApproval.
    ///
    /// When `roles` is provided, backend selection uses role→backend resolution
    /// (enabling any coding agent to be swapped in via config). When `None`,
    /// falls back to capability-based selection (first Agent, first Chat).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_task_pipeline(
        task_store: &TaskStore<'_>,
        gate_store: &GateStore<'_>,
        repos_config: &ReposConfig,
        agents_dir: &Path,
        registry: &BackendRegistry,
        roles: Option<&thrum_core::role::RolesConfig>,
        event_bus: &EventBus,
        budget: &Arc<Mutex<BudgetTracker>>,
        mut task: Task,
    ) -> Result<()> {
        let repo_config = repos_config
            .get(&task.repo)
            .context(format!("no config for repo {}", task.repo))?;

        // Role-aware backend selection: resolve implementer role → backend
        let (agent, impl_role_name, impl_budget_usd) = if let Some(roles) = roles {
            let impl_role = roles.implementer();
            let backend = registry
                .resolve_role(&impl_role)
                .context("no backend available for implementer role")?;
            let budget_usd = impl_role.budget_usd.unwrap_or(6.0);
            (backend, impl_role.backend.clone(), budget_usd)
        } else {
            let backend = registry.agent().context("no agent backend available")?;
            (backend, "default-agent".to_string(), 6.0)
        };

        tracing::info!(
            role = "implementer",
            backend = agent.name(),
            model = agent.model(),
            role_backend = %impl_role_name,
            "selected backend for implementation"
        );

        // --- Budget check: ensure enough remaining before starting ---
        {
            let tracker = budget.lock().await;
            if !tracker.can_afford(impl_budget_usd) {
                tracing::warn!(
                    task_id = %task.id,
                    remaining_usd = tracker.remaining(),
                    required_usd = impl_budget_usd,
                    ceiling_usd = tracker.ceiling_usd,
                    "budget exhausted, skipping task"
                );
                return Ok(());
            }
        }

        // --- Implement ---
        let branch = task.branch_name();
        let prev_status = task.status.label().to_string();
        task.status = TaskStatus::Implementing {
            branch: branch.clone(),
            started_at: Utc::now(),
        };
        task.updated_at = Utc::now();
        task_store.update(&task)?;
        emit_state_change(event_bus, &task, &prev_status, "implementing");

        let git = GitRepo::open(&repo_config.path)?;
        git.create_branch(&branch)?;

        let agent_file = agents_dir.join(format!("implementer_{}.md", task.repo));
        let system_prompt = load_agent_prompt(&agent_file, repo_config.claude_md.as_deref())
            .await
            .unwrap_or_default();

        // Inject relevant memories as context
        let memory_context = {
            let memory_store = thrum_db::memory_store::MemoryStore::new(task_store.db());
            match memory_store.query_for_task(&task.repo, 5) {
                Ok(memories) if !memories.is_empty() => {
                    let ctx: Vec<String> = memories.iter().map(|m| m.to_prompt_context()).collect();
                    format!(
                        "\n\n## Relevant context from previous sessions\n{}",
                        ctx.join("\n")
                    )
                }
                _ => String::new(),
            }
        };

        let prompt = format!(
            "{}{memory_context}",
            build_implementation_prompt(&task, &branch)
        );

        let request = AiRequest::new(&prompt)
            .with_system(system_prompt)
            .with_cwd(repo_config.path.clone());

        let result = agent.invoke(&request).await?;

        // Record implementation cost
        record_invocation_cost(
            budget,
            task.id.0,
            SessionType::Implementation,
            &result,
            impl_budget_usd,
        )
        .await;

        if result.timed_out || result.exit_code.is_some_and(|c| c != 0) {
            tracing::warn!(
                timed_out = result.timed_out,
                exit_code = ?result.exit_code,
                "implementation session had issues"
            );
        }

        // --- Gate 1: Quality ---
        tracing::info!("running Gate 1: Quality");
        event_bus.emit(EventKind::GateStarted {
            task_id: task.id.clone(),
            level: GateLevel::Quality,
        });
        let gate1 = run_gate(&GateLevel::Quality, repo_config)?;
        gate_store.store(&task.id, &gate1)?;
        event_bus.emit(EventKind::GateFinished {
            task_id: task.id.clone(),
            level: GateLevel::Quality,
            passed: gate1.passed,
            duration_secs: gate1.duration_secs,
        });

        if !gate1.passed {
            emit_state_change(event_bus, &task, "implementing", "gate1_failed");
            task.status = TaskStatus::Gate1Failed { report: gate1 };
            task.updated_at = Utc::now();
            task_store.update(&task)?;

            // Store failure as memory for future context
            if let TaskStatus::Gate1Failed { ref report } = task.status {
                let error_summary: String = report
                    .checks
                    .iter()
                    .filter(|c| !c.passed)
                    .map(|c| {
                        format!(
                            "{}: {}",
                            c.name,
                            c.stderr.chars().take(200).collect::<String>()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let mem = thrum_core::memory::MemoryEntry::new(
                    task.id.clone(),
                    task.repo.clone(),
                    thrum_core::memory::MemoryCategory::Error {
                        error_type: "gate1_failure".into(),
                    },
                    error_summary,
                );
                let memory_store = thrum_db::memory_store::MemoryStore::new(task_store.db());
                let _ = memory_store.store(&mem);
            }

            return Ok(());
        }

        // --- Review (role-aware backend selection) ---
        let (reviewer, review_budget_usd): (&dyn AiBackend, f64) = if let Some(roles) = roles {
            let rev_role = roles.reviewer();
            let budget_usd = rev_role.budget_usd.unwrap_or(1.0);
            let backend = registry
                .resolve_role(&rev_role)
                .or_else(|| registry.chat())
                .or_else(|| registry.agent())
                .context("no backend available for reviewer role")?;
            (backend, budget_usd)
        } else {
            let backend = registry
                .chat()
                .or_else(|| registry.agent())
                .context("no backend available for review")?;
            (backend, 1.0)
        };

        tracing::info!(
            role = "reviewer",
            backend = reviewer.name(),
            model = reviewer.model(),
            "selected backend for review"
        );

        let reviewer_prompt_file = agents_dir.join("reviewer.md");
        let reviewer_system = load_agent_prompt(&reviewer_prompt_file, None)
            .await
            .unwrap_or_default();

        let diff = git.diff_summary().unwrap_or_default();
        let review_request = AiRequest::new(format!(
            "Review this change for correctness, proof obligations, and style:\n\n{diff}"
        ))
        .with_system(reviewer_system);

        let review_result = reviewer.invoke(&review_request).await?;

        // Record review cost
        record_invocation_cost(
            budget,
            task.id.0,
            SessionType::Review,
            &review_result,
            review_budget_usd,
        )
        .await;

        emit_state_change(event_bus, &task, "implementing", "reviewing");
        task.status = TaskStatus::Reviewing {
            reviewer_output: review_result.content.clone(),
        };
        task.updated_at = Utc::now();
        task_store.update(&task)?;

        // --- Gate 2: Proofs ---
        tracing::info!("running Gate 2: Proof");
        event_bus.emit(EventKind::GateStarted {
            task_id: task.id.clone(),
            level: GateLevel::Proof,
        });
        let gate2 = run_gate(&GateLevel::Proof, repo_config)?;
        gate_store.store(&task.id, &gate2)?;
        event_bus.emit(EventKind::GateFinished {
            task_id: task.id.clone(),
            level: GateLevel::Proof,
            passed: gate2.passed,
            duration_secs: gate2.duration_secs,
        });

        if !gate2.passed {
            emit_state_change(event_bus, &task, "reviewing", "gate2_failed");
            task.status = TaskStatus::Gate2Failed { report: gate2 };
            task.updated_at = Utc::now();
            task_store.update(&task)?;

            // Store failure as memory for future context
            if let TaskStatus::Gate2Failed { ref report } = task.status {
                let error_summary: String = report
                    .checks
                    .iter()
                    .filter(|c| !c.passed)
                    .map(|c| {
                        format!(
                            "{}: {}",
                            c.name,
                            c.stderr.chars().take(200).collect::<String>()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let mem = thrum_core::memory::MemoryEntry::new(
                    task.id.clone(),
                    task.repo.clone(),
                    thrum_core::memory::MemoryCategory::Error {
                        error_type: "gate2_failure".into(),
                    },
                    error_summary,
                );
                let memory_store = thrum_db::memory_store::MemoryStore::new(task_store.db());
                let _ = memory_store.store(&mem);
            }

            return Ok(());
        }

        // --- Checkpoint: Await Human Approval ---
        let summary = CheckpointSummary {
            diff_summary: diff,
            reviewer_output: review_result.content,
            gate1_report: gate1,
            gate2_report: Some(gate2),
        };
        emit_state_change(event_bus, &task, "reviewing", "awaiting_approval");
        task.status = TaskStatus::AwaitingApproval { summary };
        task.updated_at = Utc::now();
        task_store.update(&task)?;

        tracing::info!(
            task_id = %task.id,
            "task awaiting approval — use `thrum task approve {}`",
            task.id.0
        );

        // Store successful approach as pattern memory
        {
            let mem = thrum_core::memory::MemoryEntry::new(
                task.id.clone(),
                task.repo.clone(),
                thrum_core::memory::MemoryCategory::Pattern {
                    pattern_name: "successful_implementation".into(),
                },
                format!("Task '{}' passed gates and reached approval", task.title),
            );
            let memory_store = thrum_db::memory_store::MemoryStore::new(task_store.db());
            let _ = memory_store.store(&mem);
        }

        Ok(())
    }

    /// Post-approval: Approved → Integrating → Gate 3 → Merged.
    ///
    /// `integration_steps`: if non-empty, runs config-driven integration gate.
    /// If empty, Gate 3 passes vacuously (single-repo or no integration configured).
    pub async fn post_approval_pipeline(
        task_store: &TaskStore<'_>,
        gate_store: &GateStore<'_>,
        repos_config: &ReposConfig,
        event_bus: &EventBus,
        integration_steps: &[thrum_core::gate::IntegrationStep],
        mut task: Task,
    ) -> Result<()> {
        let repo_config = repos_config
            .get(&task.repo)
            .context(format!("no config for repo {}", task.repo))?;

        emit_state_change(event_bus, &task, "approved", "integrating");
        task.status = TaskStatus::Integrating;
        task.updated_at = Utc::now();
        task_store.update(&task)?;

        // --- Gate 3: Integration pipeline ---
        tracing::info!("running Gate 3: Integration");
        event_bus.emit(EventKind::GateStarted {
            task_id: task.id.clone(),
            level: GateLevel::Integration,
        });

        let gate3 = if integration_steps.is_empty() {
            // No integration steps configured — pass vacuously
            tracing::info!("no integration steps configured, Gate 3 passes vacuously");
            thrum_core::task::GateReport {
                level: GateLevel::Integration,
                checks: vec![thrum_core::task::CheckResult {
                    name: "no_integration_steps".into(),
                    passed: true,
                    stdout: "No integration steps configured for this pipeline".into(),
                    stderr: String::new(),
                    exit_code: 0,
                }],
                passed: true,
                duration_secs: 0.0,
            }
        } else {
            let config = thrum_core::gate::IntegrationGateConfig {
                steps: integration_steps.to_vec(),
            };
            let fixture = PathBuf::from("fixtures/pipeline_test.wat");
            run_integration_gate_configured(repos_config, &fixture, &config)?
        };

        gate_store.store(&task.id, &gate3)?;
        event_bus.emit(EventKind::GateFinished {
            task_id: task.id.clone(),
            level: GateLevel::Integration,
            passed: gate3.passed,
            duration_secs: gate3.duration_secs,
        });

        if !gate3.passed {
            emit_state_change(event_bus, &task, "integrating", "gate3_failed");
            task.status = TaskStatus::Gate3Failed { report: gate3 };
            task.updated_at = Utc::now();
            task_store.update(&task)?;
            tracing::warn!(task_id = %task.id, "Gate 3 failed — integration pipeline broken");
            return Ok(());
        }

        // --- Merge ---
        let branch = task.branch_name();
        tracing::info!(branch = %branch, "merging branch to main");
        let git = GitRepo::open(&repo_config.path)?;
        let commit_sha = git
            .merge_branch_to_main(&branch)
            .context("failed to merge branch")?;

        emit_state_change(event_bus, &task, "integrating", "merged");
        task.status = TaskStatus::Merged {
            commit_sha: commit_sha.clone(),
        };
        task.updated_at = Utc::now();
        task_store.update(&task)?;

        tracing::info!(
            task_id = %task.id,
            commit = %commit_sha,
            "task merged successfully"
        );

        Ok(())
    }

    /// Retry a failed task: bump retry count, re-invoke with gate failure feedback.
    #[allow(clippy::too_many_arguments)]
    pub async fn retry_task_pipeline(
        task_store: &TaskStore<'_>,
        gate_store: &GateStore<'_>,
        repos_config: &ReposConfig,
        agents_dir: &Path,
        registry: &BackendRegistry,
        event_bus: &EventBus,
        budget: &Arc<Mutex<BudgetTracker>>,
        mut task: Task,
    ) -> Result<()> {
        let feedback = match &task.status {
            TaskStatus::Gate1Failed { report } => {
                let failed_checks: Vec<_> = report
                    .checks
                    .iter()
                    .filter(|c| !c.passed)
                    .map(|c| {
                        format!(
                            "{}: {}",
                            c.name,
                            c.stderr.chars().take(500).collect::<String>()
                        )
                    })
                    .collect();
                format!(
                    "Gate 1 (Quality) failed. Fix these issues:\n{}",
                    failed_checks.join("\n")
                )
            }
            TaskStatus::Gate2Failed { report } => {
                let failed_checks: Vec<_> = report
                    .checks
                    .iter()
                    .filter(|c| !c.passed)
                    .map(|c| {
                        format!(
                            "{}: {}",
                            c.name,
                            c.stderr.chars().take(500).collect::<String>()
                        )
                    })
                    .collect();
                format!(
                    "Gate 2 (Proof) failed. Fix these issues:\n{}",
                    failed_checks.join("\n")
                )
            }
            TaskStatus::Claimed { .. } => {
                // Was claimed from a retryable state — check original retry count
                "Previous gate failure (details in prior attempts).".to_string()
            }
            _ => return Ok(()),
        };

        task.retry_count += 1;
        task.status = TaskStatus::Pending;
        task.updated_at = Utc::now();
        task_store.update(&task)?;

        let original_desc = task.description.clone();
        task.description = format!(
            "{original_desc}\n\n---\n**RETRY {}/{}** — Previous attempt failed:\n{feedback}",
            task.retry_count, MAX_RETRIES
        );

        run_task_pipeline(
            task_store,
            gate_store,
            repos_config,
            agents_dir,
            registry,
            None,
            event_bus,
            budget,
            task,
        )
        .await
    }

    pub fn build_implementation_prompt(task: &Task, branch: &str) -> String {
        if let Some(ref spec) = task.spec {
            format!(
                "Implement the following specification on branch '{branch}':\n\n{}\n\n\
                 Follow the CLAUDE.md conventions for this repo exactly.",
                spec.to_markdown()
            )
        } else {
            format!(
                "Implement the following task on branch '{branch}':\n\n\
                 **Title**: {}\n\
                 **Description**: {}\n\
                 **Acceptance Criteria**:\n{}\n\n\
                 Follow the CLAUDE.md conventions for this repo exactly.",
                task.title,
                task.description,
                task.acceptance_criteria
                    .iter()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    }
}
