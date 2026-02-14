//! Parallel agent execution engine.
//!
//! Dispatches multiple agents concurrently with:
//! - Global semaphore capping total concurrent agents
//! - Per-repo semaphore (1 per repo) preventing git working directory conflicts
//! - Atomic task claiming via redb single-writer transactions
//! - Graceful shutdown via CancellationToken

use crate::backend::BackendRegistry;
use crate::coordination_hub::CoordinationHub;
use crate::event_bus::EventBus;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thrum_core::agent::{AgentId, AgentSession};
use thrum_core::budget::BudgetTracker;
use thrum_core::coordination::ConflictPolicy;
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
    /// Test subsampling configuration (None = no subsampling).
    pub subsample: Option<thrum_core::subsample::SubsampleConfig>,
    /// Base directory for git worktrees (used when `per_repo_limit > 1`).
    /// Defaults to `./worktrees` relative to the process working directory.
    pub worktrees_dir: PathBuf,
    /// Agent-to-agent coordination hub for parallel execution.
    /// Manages file conflict detection, shared memory, and cross-agent notifications.
    pub coordination: CoordinationHub,
    /// Policy for handling file conflicts between concurrent agents.
    pub conflict_policy: ConflictPolicy,
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

    // Start the coordination conflict listener as a background task.
    // It watches for FileChanged events and detects overlapping file access.
    let coordination_cancel = CancellationToken::new();
    let coordination_handle = ctx
        .coordination
        .start_conflict_listener(coordination_cancel.clone());

    tracing::info!(
        max_agents = config.max_agents,
        per_repo = config.per_repo_limit,
        conflict_policy = ?ctx.conflict_policy,
        "parallel engine started"
    );

    ctx.event_bus.emit(EventKind::EngineLog {
        level: thrum_core::event::LogLevel::Info,
        message: format!(
            "parallel engine started (max_agents={}, per_repo={}, conflict_policy={:?})",
            config.max_agents, config.per_repo_limit, ctx.conflict_policy
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

    // Stop the coordination conflict listener and log a summary.
    coordination_cancel.cancel();
    let _ = coordination_handle.await;
    {
        let summary = ctx.coordination.summary().await;
        if summary.conflicts_count > 0 {
            tracing::warn!(
                conflicts = summary.conflicts_count,
                "coordination: file conflicts detected during session"
            );
        }
        tracing::info!(coordination = %summary, "coordination session summary");
    }

    // Lifecycle: decay and prune stale memory entries at engine shutdown.
    // Uses 72-hour half-life (memories lose half their relevance every 3 days)
    // and prunes entries that have decayed below 0.05 (effectively forgotten).
    {
        let memory_store = thrum_db::memory_store::MemoryStore::new(&ctx.db);
        match memory_store.decay_all(72.0) {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(entries = n, "decayed memory relevance scores");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to decay memory entries"),
        }
        match memory_store.prune_below(0.05) {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(pruned = n, "pruned low-relevance memory entries");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to prune memory entries"),
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
///
/// When `per_repo_limit > 1`, each agent is given its own git worktree for
/// isolated concurrent work on the same repository. With `per_repo_limit == 1`
/// agents use the main repo working directory (no worktree overhead).
async fn dispatch_batch(
    ctx: &Arc<PipelineContext>,
    config: &EngineConfig,
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
    let use_worktrees = config.per_repo_limit > 1;

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

            // Create worktree for isolation when running multiple agents per repo.
            // The worktree is moved into the spawned task and auto-cleaned on drop.
            let (work_dir, worktree) = if use_worktrees {
                let branch = task.branch_name();
                let git = crate::git::GitRepo::open(&repo_config.path)?;

                // Ensure the branch exists before creating the worktree
                if git.current_branch().ok().as_deref() != Some(&branch) {
                    let _ = git.create_branch(&branch);
                }

                let wt = git.create_worktree(&branch, &ctx.worktrees_dir)?;
                let path = wt.path.clone();
                tracing::info!(
                    agent = %agent_id,
                    worktree = %path.display(),
                    "created worktree for agent isolation"
                );
                (path, Some(wt))
            } else {
                (repo_config.path.clone(), None)
            };

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
                work_dir = %session.work_dir.display(),
                "dispatching agent"
            );

            // Register the agent with the coordination hub for conflict detection.
            ctx.coordination
                .register_agent(agent_id.clone(), task.id.clone(), task.repo.clone())
                .await;

            ctx.event_bus.emit(EventKind::AgentStarted {
                agent_id: agent_id.clone(),
                task_id: task.id.clone(),
                repo: task.repo.clone(),
            });

            let ctx = Arc::clone(ctx);
            let category_copy = category;

            join_set.spawn(async move {
                let mut session = session;
                let outcome = run_agent_task(&ctx, task, category_copy, worktree.as_ref()).await;
                session.finish();
                // Unregister from coordination hub — clears file ownership tracking.
                ctx.coordination.unregister_agent(&session.agent_id).await;
                // Permits are dropped when this future completes, releasing semaphores.
                // The worktree (if any) is dropped here, triggering auto-cleanup.
                drop(global_permit);
                drop(repo_permit);
                drop(worktree);
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
///
/// When a `worktree` is provided (parallel multi-agent mode), the agent uses
/// the worktree path as its working directory instead of the main repo path.
///
/// A [`FileWatcher`](crate::watcher::FileWatcher) is started on the working
/// directory before the pipeline runs and stopped when it completes.
async fn run_agent_task(
    ctx: &PipelineContext,
    task: Task,
    category: ClaimCategory,
    worktree: Option<&crate::worktree::Worktree>,
) -> Result<()> {
    let task_store = TaskStore::new(&ctx.db);
    let gate_store = thrum_db::gate_store::GateStore::new(&ctx.db);

    let roles_ref = ctx.roles.as_deref();

    // Determine the effective working directory: worktree path (if isolated)
    // or main repo path (single-agent mode).
    let work_dir = worktree.map(|wt| wt.path.clone());

    // Start file watcher for real-time change detection
    let agent_id = AgentId::generate(&task.repo, &task.id);
    let repo_config = ctx.repos_config.get(&task.repo);
    let watch_dir = work_dir
        .clone()
        .or_else(|| repo_config.map(|rc| rc.path.clone()));
    let watcher = if let Some(dir) = watch_dir {
        match crate::watcher::FileWatcher::start(
            dir,
            agent_id,
            task.id.clone(),
            ctx.event_bus.clone(),
        ) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!(error = %e, "failed to start file watcher, continuing without it");
                None
            }
        }
    } else {
        None
    };

    let result = match category {
        ClaimCategory::RetryableFailed => {
            crate::parallel::pipeline::retry_task_pipeline(
                &task_store,
                &gate_store,
                &ctx.repos_config,
                &ctx.agents_dir,
                &ctx.registry,
                &ctx.event_bus,
                &ctx.budget,
                ctx.subsample.as_ref(),
                task,
                work_dir.as_deref(),
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
                work_dir.as_deref(),
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
                ctx.subsample.as_ref(),
                task,
                work_dir.as_deref(),
            )
            .await
        }
    };

    // Stop the file watcher now that the pipeline is done.
    if let Some(w) = watcher {
        w.stop().await;
    }

    result
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
    use thrum_core::checkpoint::Checkpoint;
    use thrum_core::event::EventKind;
    use thrum_core::gate::{run_gate, run_integration_gate_configured};
    use thrum_core::repo::ReposConfig;
    use thrum_core::subsample::SubsampleConfig;
    use thrum_core::task::{CheckpointSummary, GateLevel, MAX_RETRIES, Task, TaskStatus};
    use thrum_db::checkpoint_store::CheckpointStore;
    use thrum_db::gate_store::GateStore;
    use thrum_db::session_store::SessionStore;
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

    /// Save a checkpoint and emit a CheckpointSaved event.
    fn save_checkpoint(
        checkpoint_store: &CheckpointStore<'_>,
        event_bus: &EventBus,
        checkpoint: &Checkpoint,
    ) {
        if let Err(e) = checkpoint_store.save(checkpoint) {
            tracing::warn!(
                task_id = %checkpoint.task_id,
                phase = %checkpoint.completed_phase,
                error = %e,
                "failed to save checkpoint"
            );
        } else {
            tracing::info!(
                task_id = %checkpoint.task_id,
                phase = %checkpoint.completed_phase,
                "checkpoint saved"
            );
            event_bus.emit(EventKind::CheckpointSaved {
                task_id: checkpoint.task_id.clone(),
                repo: checkpoint.repo.clone(),
                phase: checkpoint.completed_phase.clone(),
            });
        }
    }

    /// Remove a checkpoint after a task reaches a terminal state (merged)
    /// or moves to AwaitingApproval (checkpoint data is now in the task status).
    fn remove_checkpoint(checkpoint_store: &CheckpointStore<'_>, task: &Task) {
        if let Err(e) = checkpoint_store.remove(&task.id) {
            tracing::debug!(
                task_id = %task.id,
                error = %e,
                "failed to remove checkpoint (non-fatal)"
            );
        }
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
    ///
    /// When `subsample` is `Some` and enabled, test commands at each gate are
    /// wrapped through `subsample_test_cmd()` using the configured ratios.
    ///
    /// When `work_dir` is `Some`, all operations (git, gates, AI invocations)
    /// run in the worktree directory instead of the main repo path.
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
        subsample: Option<&SubsampleConfig>,
        mut task: Task,
        work_dir: Option<&Path>,
    ) -> Result<()> {
        let base_repo_config = repos_config
            .get(&task.repo)
            .context(format!("no config for repo {}", task.repo))?;

        // If a worktree work_dir is provided, override the repo path so that
        // all gate checks, git operations, and AI invocations use it.
        let repo_config = match work_dir {
            Some(dir) => base_repo_config.with_work_dir(dir.to_path_buf()),
            None => base_repo_config.clone(),
        };
        let repo_config = &repo_config;

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

        // Inject relevant memories as context.
        // Touch accessed entries so frequently-used memories maintain higher
        // relevance scores under exponential decay.
        let memory_context = {
            let memory_store = thrum_db::memory_store::MemoryStore::new(task_store.db());
            match memory_store.query_for_task(&task.repo, 5) {
                Ok(memories) if !memories.is_empty() => {
                    let ids: Vec<_> = memories.iter().map(|m| m.id.clone()).collect();
                    let _ = memory_store.touch_entries(&ids);

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

        // Look up a previous session ID for session continuation on retries.
        // If this task was retried, the session store may contain the session
        // ID from the prior (timed-out or failed) invocation.
        let session_store = SessionStore::new(task_store.db());
        let resume_session_id = session_store.get(&task.id).unwrap_or(None);
        if let Some(ref sid) = resume_session_id {
            tracing::info!(
                task_id = %task.id,
                session_id = sid,
                "found previous session ID for continuation"
            );
            event_bus.emit(EventKind::SessionContinued {
                task_id: task.id.clone(),
                repo: task.repo.clone(),
                session_id: sid.clone(),
            });
        }

        let mut request = AiRequest::new(&prompt)
            .with_system(system_prompt)
            .with_cwd(repo_config.path.clone());
        if let Some(sid) = resume_session_id {
            request = request.with_resume_session(sid);
        }

        let result = agent.invoke(&request).await?;

        // Store the session ID for potential future retries (timeout/failure recovery).
        // This persists even if the invocation timed out — especially important then,
        // since the agent's partial work is preserved in the session.
        if let Some(ref sid) = result.session_id
            && let Err(e) = session_store.save(&task.id, sid)
        {
            tracing::warn!(error = %e, "failed to store session ID");
        }

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
        let checkpoint_store = CheckpointStore::new(task_store.db());
        tracing::info!("running Gate 1: Quality");
        event_bus.emit(EventKind::GateStarted {
            task_id: task.id.clone(),
            level: GateLevel::Quality,
        });
        let gate1 = run_gate(&GateLevel::Quality, repo_config, subsample, Some(task.id.0))?;
        gate_store.store(&task.id, &gate1)?;
        event_bus.emit(EventKind::GateFinished {
            task_id: task.id.clone(),
            level: GateLevel::Quality,
            passed: gate1.passed,
            duration_secs: gate1.duration_secs,
        });

        if !gate1.passed {
            emit_state_change(event_bus, &task, "implementing", "gate1_failed");
            task.status = TaskStatus::Gate1Failed {
                report: gate1.clone(),
            };
            task.updated_at = Utc::now();
            task_store.update(&task)?;

            // Store failure as memory for future context.
            // Include the task title so the agent can correlate errors with
            // specific features when working on similar tasks later.
            let failed_checks: String = gate1
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
            let error_summary = format!(
                "Task '{}' failed Gate 1 (Quality): {failed_checks}",
                task.title
            );
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

            // Record failure signatures for convergence detection.
            // On retry, the convergence store is read to determine if the
            // agent is stuck repeating the same failure.
            record_convergence_failures(task_store.db(), &task.id, &gate1);

            return Ok(());
        }

        // --- Checkpoint: Gate 1 passed ---
        {
            let mut cp = Checkpoint::after_implementation(
                task.id.clone(),
                task.repo.clone(),
                branch.clone(),
            );
            cp.advance_to_gate1(gate1.clone());
            save_checkpoint(&checkpoint_store, event_bus, &cp);
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

        // --- Checkpoint: Review completed ---
        {
            let cp_store = CheckpointStore::new(task_store.db());
            match cp_store.get(&task.id) {
                Ok(Some(mut cp)) => {
                    cp.advance_to_review(review_result.content.clone());
                    save_checkpoint(&cp_store, event_bus, &cp);
                }
                _ => {
                    // No existing checkpoint — create a fresh one with review state
                    let mut cp = Checkpoint::after_implementation(
                        task.id.clone(),
                        task.repo.clone(),
                        branch.clone(),
                    );
                    cp.advance_to_gate1(gate1.clone());
                    cp.advance_to_review(review_result.content.clone());
                    save_checkpoint(&cp_store, event_bus, &cp);
                }
            }
        }

        // --- Gate 2: Proofs ---
        tracing::info!("running Gate 2: Proof");
        event_bus.emit(EventKind::GateStarted {
            task_id: task.id.clone(),
            level: GateLevel::Proof,
        });
        let gate2 = run_gate(&GateLevel::Proof, repo_config, subsample, Some(task.id.0))?;
        gate_store.store(&task.id, &gate2)?;
        event_bus.emit(EventKind::GateFinished {
            task_id: task.id.clone(),
            level: GateLevel::Proof,
            passed: gate2.passed,
            duration_secs: gate2.duration_secs,
        });

        if !gate2.passed {
            emit_state_change(event_bus, &task, "reviewing", "gate2_failed");
            task.status = TaskStatus::Gate2Failed {
                report: gate2.clone(),
            };
            task.updated_at = Utc::now();
            task_store.update(&task)?;

            // Store failure as memory for future context
            let failed_checks: String = gate2
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
            let error_summary = format!(
                "Task '{}' failed Gate 2 (Proof): {failed_checks}",
                task.title
            );
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

            // Record failure signatures for convergence detection.
            record_convergence_failures(task_store.db(), &task.id, &gate2);

            return Ok(());
        }

        // --- Checkpoint: Gate 2 passed ---
        {
            let cp_store = CheckpointStore::new(task_store.db());
            match cp_store.get(&task.id) {
                Ok(Some(mut cp)) => {
                    cp.advance_to_gate2(gate2.clone());
                    save_checkpoint(&cp_store, event_bus, &cp);
                }
                _ => {
                    let mut cp = Checkpoint::after_implementation(
                        task.id.clone(),
                        task.repo.clone(),
                        branch.clone(),
                    );
                    cp.advance_to_gate1(gate1.clone());
                    cp.advance_to_review(review_result.content.clone());
                    cp.advance_to_gate2(gate2.clone());
                    save_checkpoint(&cp_store, event_bus, &cp);
                }
            }
        }

        // --- Await Human Approval ---
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

        // Clean up checkpoint and session — task state now captures all needed data
        remove_checkpoint(&checkpoint_store, &task);
        let _ = session_store.remove(&task.id);

        tracing::info!(
            task_id = %task.id,
            "task awaiting approval — use `thrum task approve {}`",
            task.id.0
        );

        // Store successful approach as pattern memory.
        // Include acceptance criteria so future similar tasks can reference
        // what a successful implementation looked like.
        {
            let criteria_summary = if task.acceptance_criteria.is_empty() {
                String::new()
            } else {
                format!(" (criteria: {})", task.acceptance_criteria.join(", "))
            };
            let mem = thrum_core::memory::MemoryEntry::new(
                task.id.clone(),
                task.repo.clone(),
                thrum_core::memory::MemoryCategory::Pattern {
                    pattern_name: "successful_implementation".into(),
                },
                format!(
                    "Task '{}' passed gates and reached approval{criteria_summary}",
                    task.title
                ),
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
    ///
    /// When `work_dir` is `Some`, merge operations use the worktree path.
    pub async fn post_approval_pipeline(
        task_store: &TaskStore<'_>,
        gate_store: &GateStore<'_>,
        repos_config: &ReposConfig,
        event_bus: &EventBus,
        integration_steps: &[thrum_core::gate::IntegrationStep],
        mut task: Task,
        work_dir: Option<&Path>,
    ) -> Result<()> {
        let base_repo_config = repos_config
            .get(&task.repo)
            .context(format!("no config for repo {}", task.repo))?;

        let repo_config = match work_dir {
            Some(dir) => base_repo_config.with_work_dir(dir.to_path_buf()),
            None => base_repo_config.clone(),
        };
        let repo_config = &repo_config;

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

        // Clean up any stale checkpoint and session for this task
        let checkpoint_store = CheckpointStore::new(task_store.db());
        remove_checkpoint(&checkpoint_store, &task);
        let _ = SessionStore::new(task_store.db()).remove(&task.id);

        tracing::info!(
            task_id = %task.id,
            commit = %commit_sha,
            "task merged successfully"
        );

        Ok(())
    }

    /// Retry a failed task with convergence-aware strategy rotation.
    ///
    /// Instead of blind retries, analyzes failure history to detect convergence
    /// (the agent repeating the same failure). When convergence is detected,
    /// the strategy escalates:
    /// 1. Normal: standard retry with failure feedback
    /// 2. ExpandedContext: more detail, explicit "read the full error" directive
    /// 3. DifferentApproach: "your approach is NOT working, try something else"
    /// 4. HumanReview: stops automatic retry, flags for human intervention
    ///
    /// Also queries failure-specific memories from the memory store for context.
    #[allow(clippy::too_many_arguments)]
    pub async fn retry_task_pipeline(
        task_store: &TaskStore<'_>,
        gate_store: &GateStore<'_>,
        repos_config: &ReposConfig,
        agents_dir: &Path,
        registry: &BackendRegistry,
        event_bus: &EventBus,
        budget: &Arc<Mutex<BudgetTracker>>,
        subsample: Option<&SubsampleConfig>,
        mut task: Task,
        work_dir: Option<&Path>,
    ) -> Result<()> {
        use thrum_core::convergence::RetryStrategy;

        let (feedback, convergence_augmentation) = match &task.status {
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
                let feedback = format!(
                    "Gate 1 (Quality) failed. Fix these issues:\n{}",
                    failed_checks.join("\n")
                );

                // Convergence analysis: compare new failure against history
                let augmentation =
                    analyze_convergence(task_store.db(), &task.id, report, event_bus);
                (feedback, augmentation)
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
                let feedback = format!(
                    "Gate 2 (Proof) failed. Fix these issues:\n{}",
                    failed_checks.join("\n")
                );

                let augmentation =
                    analyze_convergence(task_store.db(), &task.id, report, event_bus);
                (feedback, augmentation)
            }
            TaskStatus::Claimed { .. } => {
                // Was claimed from a retryable state — check original retry count
                let feedback = "Previous gate failure (details in prior attempts).".to_string();
                (feedback, ConvergenceAugmentation::normal())
            }
            _ => return Ok(()),
        };

        // If convergence analysis says human review is needed, don't retry.
        // The task stays in its failed state and will not be automatically claimed.
        if convergence_augmentation.strategy == RetryStrategy::HumanReview {
            tracing::warn!(
                task_id = %task.id,
                strategy = "human-review",
                "convergence detected: flagging task for human review instead of retrying"
            );
            event_bus.emit(EventKind::TaskConvergenceDetected {
                task_id: task.id.clone(),
                strategy: "human-review".into(),
                repeated_count: convergence_augmentation.max_occurrence,
            });
            // Don't increment retry_count or reset status — leave in failed state
            return Ok(());
        }

        tracing::info!(
            task_id = %task.id,
            strategy = convergence_augmentation.strategy.label(),
            repeated_failures = convergence_augmentation.repeated_count,
            "retrying with convergence-aware strategy"
        );

        // Query failure-specific memories for context-aware retries.
        // These are error-category memories from the same repo, surfacing
        // patterns like "cargo fmt failed" or "proof obligation missing"
        // that help the agent avoid repeating past mistakes.
        let failure_memories = {
            let memory_store = thrum_db::memory_store::MemoryStore::new(task_store.db());
            match memory_store.query_errors_for_repo(&task.repo, 5) {
                Ok(memories) if !memories.is_empty() => {
                    // Touch accessed memories to maintain their relevance
                    let ids: Vec<_> = memories.iter().map(|m| m.id.clone()).collect();
                    let _ = memory_store.touch_entries(&ids);

                    let ctx: Vec<String> = memories.iter().map(|m| m.to_prompt_context()).collect();
                    format!(
                        "\n\n## Failure-specific context from previous attempts\n{}",
                        ctx.join("\n")
                    )
                }
                _ => String::new(),
            }
        };

        task.retry_count += 1;
        task.status = TaskStatus::Pending;
        task.updated_at = Utc::now();
        task_store.update(&task)?;

        let original_desc = task.description.clone();
        task.description = format!(
            "{original_desc}\n\n---\n**RETRY {}/{} [strategy: {}]** — Previous attempt failed:\n\
             {feedback}{failure_memories}{convergence_prompt}",
            task.retry_count,
            MAX_RETRIES,
            convergence_augmentation.strategy.label(),
            convergence_prompt = convergence_augmentation.prompt,
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
            subsample,
            task,
            work_dir,
        )
        .await
    }

    /// Resume a task from its last checkpoint, skipping already-completed gates.
    ///
    /// Detects the existing branch and checkpoint, then picks up the pipeline
    /// from the phase after the last checkpoint. This avoids re-running
    /// expensive AI invocations and gate checks that already passed.
    ///
    /// Returns `Ok(true)` if resumption was performed, `Ok(false)` if no
    /// checkpoint was found (caller should run the full pipeline instead).
    #[allow(clippy::too_many_arguments)]
    pub async fn resume_task_pipeline(
        task_store: &TaskStore<'_>,
        gate_store: &GateStore<'_>,
        repos_config: &ReposConfig,
        agents_dir: &Path,
        registry: &BackendRegistry,
        roles: Option<&thrum_core::role::RolesConfig>,
        event_bus: &EventBus,
        budget: &Arc<Mutex<BudgetTracker>>,
        subsample: Option<&SubsampleConfig>,
        mut task: Task,
        work_dir: Option<&Path>,
    ) -> Result<bool> {
        let checkpoint_store = CheckpointStore::new(task_store.db());
        let checkpoint = match checkpoint_store.get(&task.id)? {
            Some(cp) => cp,
            None => return Ok(false), // No checkpoint — cannot resume
        };

        let base_repo_config = repos_config
            .get(&task.repo)
            .context(format!("no config for repo {}", task.repo))?;

        let repo_config = match work_dir {
            Some(dir) => base_repo_config.with_work_dir(dir.to_path_buf()),
            None => base_repo_config.clone(),
        };
        let repo_config = &repo_config;

        let branch = checkpoint.branch.clone();

        tracing::info!(
            task_id = %task.id,
            phase = %checkpoint.completed_phase,
            branch = %branch,
            "resuming task from checkpoint"
        );

        // Ensure the task is in Implementing state for resumption
        let prev_status = task.status.label().to_string();
        task.status = TaskStatus::Implementing {
            branch: branch.clone(),
            started_at: Utc::now(),
        };
        task.updated_at = Utc::now();
        task_store.update(&task)?;
        emit_state_change(event_bus, &task, &prev_status, "implementing (resumed)");

        // Verify the branch still exists
        let git = GitRepo::open(&repo_config.path)?;
        if let Err(e) = git.create_branch(&branch) {
            tracing::debug!(
                error = %e,
                "branch already exists (expected for resume)"
            );
        }

        // Determine what we can skip based on checkpoint phase
        let gate1_report = if checkpoint.gate1_passed() {
            tracing::info!(
                task_id = %task.id,
                "skipping Gate 1 (already passed in checkpoint)"
            );
            checkpoint
                .gate1_report
                .clone()
                .context("checkpoint says gate1 passed but no report found")?
        } else {
            // Gate 1 not yet passed — run it
            tracing::info!("running Gate 1: Quality (resume)");
            event_bus.emit(EventKind::GateStarted {
                task_id: task.id.clone(),
                level: GateLevel::Quality,
            });
            let gate1 = run_gate(&GateLevel::Quality, repo_config, subsample, Some(task.id.0))?;
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
                // Remove stale checkpoint on failure
                remove_checkpoint(&checkpoint_store, &task);
                return Ok(true);
            }

            // Save checkpoint after gate1
            let mut cp = checkpoint.clone();
            cp.advance_to_gate1(gate1.clone());
            save_checkpoint(&checkpoint_store, event_bus, &cp);

            gate1
        };

        let reviewer_output = if checkpoint.review_completed() {
            tracing::info!(
                task_id = %task.id,
                "skipping review (already completed in checkpoint)"
            );
            checkpoint.reviewer_output.clone().unwrap_or_default()
        } else {
            // Run review
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

            // Save checkpoint after review
            {
                let cp_store = CheckpointStore::new(task_store.db());
                if let Ok(Some(mut cp)) = cp_store.get(&task.id) {
                    cp.advance_to_review(review_result.content.clone());
                    save_checkpoint(&cp_store, event_bus, &cp);
                }
            }

            review_result.content
        };

        let gate2_report = if checkpoint.gate2_passed() {
            tracing::info!(
                task_id = %task.id,
                "skipping Gate 2 (already passed in checkpoint)"
            );
            checkpoint.gate2_report.clone()
        } else {
            // Run Gate 2
            tracing::info!("running Gate 2: Proof (resume)");
            event_bus.emit(EventKind::GateStarted {
                task_id: task.id.clone(),
                level: GateLevel::Proof,
            });
            let gate2 = run_gate(&GateLevel::Proof, repo_config, subsample, Some(task.id.0))?;
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
                remove_checkpoint(&checkpoint_store, &task);
                return Ok(true);
            }

            // Save checkpoint after gate2
            {
                let cp_store = CheckpointStore::new(task_store.db());
                if let Ok(Some(mut cp)) = cp_store.get(&task.id) {
                    cp.advance_to_gate2(gate2.clone());
                    save_checkpoint(&cp_store, event_bus, &cp);
                }
            }

            Some(gate2)
        };

        // --- AwaitingApproval ---
        let diff = git.diff_summary().unwrap_or_default();
        let summary = CheckpointSummary {
            diff_summary: diff,
            reviewer_output,
            gate1_report,
            gate2_report,
        };
        emit_state_change(event_bus, &task, "reviewing", "awaiting_approval");
        task.status = TaskStatus::AwaitingApproval { summary };
        task.updated_at = Utc::now();
        task_store.update(&task)?;

        // Clean up checkpoint — task state now captures all needed data
        remove_checkpoint(&checkpoint_store, &task);

        tracing::info!(
            task_id = %task.id,
            "resumed task now awaiting approval — use `thrum task approve {}`",
            task.id.0
        );

        // Store successful approach as pattern memory
        {
            let criteria_summary = if task.acceptance_criteria.is_empty() {
                String::new()
            } else {
                format!(" (criteria: {})", task.acceptance_criteria.join(", "))
            };
            let mem = thrum_core::memory::MemoryEntry::new(
                task.id.clone(),
                task.repo.clone(),
                thrum_core::memory::MemoryCategory::Pattern {
                    pattern_name: "successful_implementation".into(),
                },
                format!(
                    "Task '{}' passed gates and reached approval (resumed){criteria_summary}",
                    task.title
                ),
            );
            let memory_store = thrum_db::memory_store::MemoryStore::new(task_store.db());
            let _ = memory_store.store(&mem);
        }

        Ok(true)
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

    /// Result of convergence analysis, carrying the strategy and prompt augmentation.
    struct ConvergenceAugmentation {
        strategy: thrum_core::convergence::RetryStrategy,
        prompt: String,
        repeated_count: u32,
        max_occurrence: u32,
    }

    impl ConvergenceAugmentation {
        fn normal() -> Self {
            Self {
                strategy: thrum_core::convergence::RetryStrategy::Normal,
                prompt: String::new(),
                repeated_count: 0,
                max_occurrence: 0,
            }
        }
    }

    /// Record failure signatures from a gate report into the convergence store.
    ///
    /// Called when a gate fails. Updates existing records (incrementing occurrence
    /// count) or creates new ones. This data is read during retry to detect
    /// convergence.
    fn record_convergence_failures(
        db: &redb::Database,
        task_id: &thrum_core::task::TaskId,
        report: &thrum_core::task::GateReport,
    ) {
        use thrum_core::convergence::{FailureRecord, FailureSignature};
        use thrum_db::convergence_store::ConvergenceStore;

        let store = ConvergenceStore::new(db);
        let signatures = FailureSignature::from_gate_report(report);

        for sig in signatures {
            // Find the stderr for this check
            let stderr = report
                .checks
                .iter()
                .find(|c| c.name == sig.check_name && !c.passed)
                .map(|c| c.stderr.chars().take(1000).collect::<String>())
                .unwrap_or_default();

            match store.get(task_id, &sig.error_hash) {
                Ok(Some(mut existing)) => {
                    existing.record_occurrence(stderr);
                    if let Err(e) = store.store(&existing) {
                        tracing::warn!(
                            task_id = %task_id,
                            error = %e,
                            "failed to update convergence record"
                        );
                    }
                }
                Ok(None) => {
                    let record = FailureRecord::new(task_id.clone(), sig, stderr);
                    if let Err(e) = store.store(&record) {
                        tracing::warn!(
                            task_id = %task_id,
                            error = %e,
                            "failed to store convergence record"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "failed to query convergence store"
                    );
                }
            }
        }
    }

    /// Analyze convergence for a task and produce a strategy-specific prompt augmentation.
    ///
    /// Reads historical failure records from the convergence store, compares them
    /// against the new gate report, and determines the retry strategy. Also emits
    /// a convergence event when repeated failures are detected.
    fn analyze_convergence(
        db: &redb::Database,
        task_id: &thrum_core::task::TaskId,
        report: &thrum_core::task::GateReport,
        event_bus: &EventBus,
    ) -> ConvergenceAugmentation {
        use thrum_core::convergence::ConvergenceAnalysis;
        use thrum_db::convergence_store::ConvergenceStore;

        let store = ConvergenceStore::new(db);
        let existing_records = match store.get_for_task(task_id) {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "failed to read convergence history, falling back to normal retry"
                );
                return ConvergenceAugmentation::normal();
            }
        };

        let analysis = ConvergenceAnalysis::analyze(&existing_records, report);
        let max_occurrence = analysis
            .occurrence_counts
            .values()
            .copied()
            .max()
            .unwrap_or(1);

        if !analysis.repeated_signatures.is_empty() {
            tracing::info!(
                task_id = %task_id,
                strategy = analysis.strategy.label(),
                repeated_count = analysis.repeated_signatures.len(),
                max_occurrence,
                "convergence analysis complete"
            );

            event_bus.emit(EventKind::TaskConvergenceDetected {
                task_id: task_id.clone(),
                strategy: analysis.strategy.label().to_string(),
                repeated_count: max_occurrence,
            });
        }

        let prompt = analysis
            .strategy
            .prompt_augmentation(&analysis.repeated_signatures);

        ConvergenceAugmentation {
            strategy: analysis.strategy,
            prompt,
            repeated_count: analysis.repeated_signatures.len() as u32,
            max_occurrence,
        }
    }
}
