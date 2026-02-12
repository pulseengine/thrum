use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thrum_core::budget::BudgetTracker;
use thrum_core::consistency::check_consistency;
use thrum_core::gate::{run_gate, run_integration_gate};
use thrum_core::repo::ReposConfig;
use thrum_core::spec::Spec;
use thrum_core::sphinx_needs::{NeedsJson, trace_record_to_needs};
use thrum_core::task::{GateLevel, RepoName, Task, TaskId, TaskStatus};
use thrum_core::telemetry::{TelemetryConfig, TraceFilter, TraceReader, init_telemetry};
use thrum_db::budget_store::BudgetStore;
use thrum_db::gate_store::GateStore;
use thrum_db::task_store::TaskStore;
use thrum_db::trace_store::TraceStore;
use thrum_runner::backend::{AiRequest, BackendRegistry};
use thrum_runner::claude::{ClaudeCliBackend, load_agent_prompt};
use thrum_runner::parallel::{EngineConfig, PipelineContext};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "thrum",
    about = "Autonomous AI-driven development orchestrator"
)]
struct Cli {
    /// Path to the database file.
    #[arg(long, default_value = "thrum.redb")]
    db: PathBuf,

    /// Path to repos.toml configuration.
    #[arg(long, default_value = "configs/repos.toml")]
    config: PathBuf,

    /// Path to pipeline.toml configuration (backends, roles, sandbox, gates).
    #[arg(long, default_value = "configs/pipeline.toml")]
    pipeline: PathBuf,

    /// Path to agent prompts directory.
    #[arg(long, default_value = "agents")]
    agents_dir: PathBuf,

    /// Directory for trace file storage.
    #[arg(long, default_value = "traces")]
    trace_dir: PathBuf,

    /// OTLP endpoint for OpenTelemetry export (e.g., "http://localhost:4317").
    #[arg(long)]
    otlp_endpoint: Option<String>,

    /// Output JSON-structured logs to console.
    #[arg(long)]
    json_logs: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the autonomous development loop.
    Run {
        /// Only process tasks for this repo.
        #[arg(long)]
        repo: Option<String>,
        /// Run one task then stop.
        #[arg(long)]
        once: bool,
        /// Number of parallel agents (default: 1 = sequential).
        #[arg(long, default_value = "1")]
        agents: usize,
    },
    /// Manage tasks in the queue.
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Show dashboard: tasks, consistency, budget.
    Status,
    /// Run cross-repo consistency checker.
    Check,
    /// Export traceability data.
    Trace {
        #[command(subcommand)]
        action: TraceAction,
    },
    /// View locally stored OpenTelemetry traces.
    Traces {
        #[command(subcommand)]
        action: TracesAction,
    },
    /// Show tool safety classification (TCL, ASIL, SOUP).
    Safety,
    /// Build release artifacts.
    Release {
        /// Dry run (don't actually create release).
        #[arg(long)]
        dry_run: bool,
        /// Git tag for the release.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Start the HTTP API server.
    Serve {
        /// Bind address.
        #[arg(long, default_value = "127.0.0.1:3000")]
        bind: String,
    },
}

#[derive(Subcommand)]
enum TracesAction {
    /// List recent trace events.
    List {
        #[arg(long, default_value = "50")]
        limit: usize,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        filter: Option<String>,
    },
    /// Show summary of stored trace files.
    Summary,
    /// Show full JSON of recent events (for piping to jq, etc.).
    Dump {
        #[arg(long, default_value = "100")]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum TraceAction {
    /// Export traceability as sphinx-needs JSON (needs.json).
    Export {
        #[arg(long, default_value = "docs/needs.json")]
        output: PathBuf,
        #[arg(long, default_value = "0.1.0")]
        version: String,
    },
    /// Generate RST traceability page for a tool.
    Rst {
        tool: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Show traceability gaps (requirements without tests/proofs).
    Gaps,
}

#[derive(Subcommand)]
enum TaskAction {
    /// Add a new task to the queue.
    Add {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        desc: String,
        #[arg(long)]
        requirement_id: Option<String>,
        /// Path to a TOML spec file for structured SDD.
        #[arg(long)]
        spec: Option<PathBuf>,
    },
    /// List all tasks.
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Approve a task awaiting checkpoint review.
    Approve { id: i64 },
    /// Reject a task with feedback.
    Reject {
        id: i64,
        #[arg(long)]
        feedback: String,
    },
    /// Show detailed info about a task.
    Show { id: i64 },
    /// Force-set a task's status (for operational recovery).
    SetStatus {
        id: i64,
        #[arg(long)]
        status: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize telemetry (OTel + local file traces)
    let telemetry_config = TelemetryConfig {
        service_name: "thrum".into(),
        otlp_endpoint: cli.otlp_endpoint.clone(),
        json_logs: cli.json_logs,
        log_filter: "thrum=info".into(),
        trace_dir: Some(cli.trace_dir.clone()),
    };
    let _telemetry_guard = init_telemetry(&telemetry_config)?;

    // Open database
    let db = thrum_db::open_db(&cli.db)?;

    match cli.command {
        Commands::Run { repo, once, agents } => {
            let repo_filter = repo.map(RepoName::new);
            let repos_config = ReposConfig::load(&cli.config)?;
            if agents > 1 && !once {
                cmd_run_parallel(
                    &cli.db,
                    repos_config,
                    &cli.agents_dir,
                    &cli.pipeline,
                    repo_filter,
                    agents,
                )
                .await
            } else {
                cmd_run(
                    &db,
                    &repos_config,
                    &cli.agents_dir,
                    &cli.pipeline,
                    repo_filter,
                    once,
                )
                .await
            }
        }
        Commands::Task { action } => cmd_task(&db, action),
        Commands::Trace { action } => cmd_trace(&db, action),
        Commands::Traces { action } => cmd_traces(&cli.trace_dir, action),
        Commands::Safety => cmd_safety(),
        Commands::Status => cmd_status(&db, &cli.config),
        Commands::Check => cmd_check(&cli.config),
        Commands::Release { dry_run, tag } => {
            let repos_config = ReposConfig::load(&cli.config)?;
            cmd_release(dry_run, tag, &repos_config, &db).await
        }
        Commands::Serve { bind } => {
            let state = std::sync::Arc::new(thrum_api::ApiState {
                db_path: cli.db.clone(),
                trace_dir: cli.trace_dir.clone(),
                config_path: Some(cli.config.clone()),
            });
            thrum_api::serve(state, &bind).await
        }
    }
}

// ─── Pipeline Config ─────────────────────────────────────────────────────

/// Unified pipeline configuration loaded from pipeline.toml.
///
/// All sections are optional — the engine falls back to sensible defaults
/// when a section is absent.
#[derive(serde::Deserialize, Default)]
struct PipelineConfig {
    /// Registered AI backends (coding agents + chat APIs).
    #[serde(default)]
    backends: Vec<thrum_core::role::BackendConfig>,
    /// Role definitions keyed by role name.
    #[serde(default)]
    roles: HashMap<String, thrum_core::role::AgentRole>,
    /// Sandbox configuration for agent isolation.
    #[serde(default)]
    sandbox: Option<thrum_runner::sandbox::SandboxConfig>,
    /// Gate configurations (integration steps, etc.).
    #[serde(default)]
    gates: GatesConfig,
    /// Budget configuration for AI spending limits.
    #[serde(default)]
    budget: BudgetConfig,
}

/// Budget configuration from [budget] section in pipeline.toml.
#[derive(serde::Deserialize)]
struct BudgetConfig {
    /// Overall spending ceiling in USD.
    #[serde(default = "default_budget_ceiling")]
    ceiling_usd: f64,
}

fn default_budget_ceiling() -> f64 {
    1000.0
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            ceiling_usd: default_budget_ceiling(),
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct GatesConfig {
    #[serde(default)]
    integration: Option<IntegrationGateToml>,
}

#[derive(serde::Deserialize, Default)]
struct IntegrationGateToml {
    #[serde(default)]
    steps: Vec<thrum_core::gate::IntegrationStep>,
}

impl PipelineConfig {
    /// Load from a TOML file. Returns defaults if the file doesn't exist.
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::info!(path = %path.display(), "pipeline config not found, using defaults");
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path).context(format!(
            "failed to read pipeline config: {}",
            path.display()
        ))?;
        let config: Self = toml::from_str(&contents).context(format!(
            "failed to parse pipeline config: {}",
            path.display()
        ))?;
        tracing::info!(
            backends = config.backends.len(),
            roles = config.roles.len(),
            sandbox = config.sandbox.is_some(),
            path = %path.display(),
            "loaded pipeline config"
        );
        Ok(config)
    }
}

// ─── Autonomous Loop ─────────────────────────────────────────────────────

/// Build a backend registry from pipeline config.
///
/// If `[[backends]]` are configured, uses config-driven registration.
/// Otherwise falls back to hardcoded defaults (Claude CLI + Anthropic API).
fn build_registry(pipeline: &PipelineConfig) -> Result<BackendRegistry> {
    let default_cwd = std::env::current_dir()?;

    let registry = if !pipeline.backends.is_empty() {
        // Config-driven: any coding agent can be plugged in via pipeline.toml
        thrum_runner::backend::build_registry_from_config(&pipeline.backends, &default_cwd)?
    } else {
        // Fallback: hardcoded Claude + Anthropic API (backward compatible)
        let mut registry = BackendRegistry::new();
        registry.register(Box::new(ClaudeCliBackend::new(default_cwd)));
        if let Ok(backend) =
            thrum_runner::anthropic::AnthropicApiBackend::from_env("claude-sonnet-4-5-20250929")
        {
            registry.register(Box::new(backend));
        }
        registry
    };

    tracing::info!(
        backends = ?registry.list().iter().map(|(n, c, m)| format!("{n} ({c:?}, {m})")).collect::<Vec<_>>(),
        "initialized backend registry"
    );

    Ok(registry)
}

/// Parallel execution path: dispatches multiple agents via the parallel engine.
///
/// Re-opens the DB as Arc<Database> because the parallel engine needs owned
/// shared access across spawned tasks (redb serializes writers internally).
async fn cmd_run_parallel(
    db_path: &Path,
    repos_config: ReposConfig,
    agents_dir: &Path,
    pipeline_config: &Path,
    repo_filter: Option<RepoName>,
    max_agents: usize,
) -> Result<()> {
    let pipeline = PipelineConfig::load(pipeline_config)?;
    let registry = build_registry(&pipeline)?;
    let shared_db = Arc::new(thrum_db::open_db(db_path)?);

    let roles_config = if pipeline.roles.is_empty() {
        thrum_core::role::RolesConfig::default()
    } else {
        thrum_core::role::RolesConfig {
            roles: pipeline.roles,
        }
    };

    // Load or create the budget tracker from DB, using the configured ceiling
    let budget_tracker = {
        let budget_store = BudgetStore::new(&shared_db);
        match budget_store.load()? {
            Some(mut existing) => {
                // Update ceiling in case config changed
                existing.ceiling_usd = pipeline.budget.ceiling_usd;
                existing
            }
            None => BudgetTracker::new(pipeline.budget.ceiling_usd),
        }
    };
    tracing::info!(
        ceiling_usd = budget_tracker.ceiling_usd,
        spent_usd = budget_tracker.total_spent(),
        remaining_usd = budget_tracker.remaining(),
        "budget tracker loaded"
    );
    let budget = Arc::new(tokio::sync::Mutex::new(budget_tracker));

    let event_bus = thrum_runner::event_bus::EventBus::new();

    let ctx = Arc::new(PipelineContext {
        db: shared_db.clone(),
        repos_config: Arc::new(repos_config),
        agents_dir: agents_dir.to_path_buf(),
        registry: Arc::new(registry),
        session_budget_usd: None,
        budget: budget.clone(),
        roles: Some(Arc::new(roles_config)),
        sandbox_config: pipeline.sandbox,
        event_bus,
        integration_steps: pipeline
            .gates
            .integration
            .as_ref()
            .map(|g| g.steps.clone())
            .unwrap_or_default(),
    });

    let config = EngineConfig {
        max_agents,
        per_repo_limit: 1,
        session_budget_usd: None,
        poll_interval: std::time::Duration::from_secs(5),
    };

    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    // Signal handler for graceful shutdown
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("received Ctrl+C, initiating graceful shutdown");
            shutdown_clone.cancel();
        }
    });

    tracing::info!(agents = max_agents, "starting parallel execution engine");
    let result = thrum_runner::parallel::run_parallel(ctx, config, repo_filter, shutdown).await;

    // Persist budget tracker to DB after run completes
    {
        let tracker = budget.lock().await;
        let budget_store = BudgetStore::new(&shared_db);
        if let Err(e) = budget_store.save(&tracker) {
            tracing::warn!(error = %e, "failed to persist budget tracker");
        } else {
            tracing::info!(
                spent_usd = tracker.total_spent(),
                remaining_usd = tracker.remaining(),
                "budget tracker persisted"
            );
        }
    }

    result
}

async fn cmd_run(
    db: &redb::Database,
    repos_config: &ReposConfig,
    agents_dir: &Path,
    pipeline_config: &Path,
    repo_filter: Option<RepoName>,
    once: bool,
) -> Result<()> {
    let task_store = TaskStore::new(db);
    let gate_store = GateStore::new(db);
    let event_bus = thrum_runner::event_bus::EventBus::new();

    let pipeline = PipelineConfig::load(pipeline_config)?;
    let registry = build_registry(&pipeline)?;
    let integration_steps = pipeline
        .gates
        .integration
        .as_ref()
        .map(|g| g.steps.clone())
        .unwrap_or_default();

    // Load or create budget tracker
    let budget_tracker = {
        let budget_store = BudgetStore::new(db);
        match budget_store.load()? {
            Some(mut existing) => {
                existing.ceiling_usd = pipeline.budget.ceiling_usd;
                existing
            }
            None => BudgetTracker::new(pipeline.budget.ceiling_usd),
        }
    };
    tracing::info!(
        ceiling_usd = budget_tracker.ceiling_usd,
        spent_usd = budget_tracker.total_spent(),
        remaining_usd = budget_tracker.remaining(),
        "budget tracker loaded"
    );
    let budget = Arc::new(tokio::sync::Mutex::new(budget_tracker));

    loop {
        // Phase A: Retry failed tasks (Gate1/Gate2 failures under retry limit)
        if let Some(task) = task_store
            .retryable_failures(repo_filter.as_ref())?
            .into_iter()
            .next()
        {
            tracing::info!(
                task_id = %task.id,
                retry = task.retry_count,
                "retrying failed task"
            );
            let result = thrum_runner::parallel::pipeline::retry_task_pipeline(
                &task_store,
                &gate_store,
                repos_config,
                agents_dir,
                &registry,
                &event_bus,
                &budget,
                task,
            )
            .await;
            if let Err(e) = result {
                tracing::error!("retry pipeline failed: {e:#}");
            }
            if once {
                break;
            }
            continue;
        }

        // Phase B: Process approved tasks (post-approval → Gate 3 → merge)
        if let Some(task) = task_store.next_approved(repo_filter.as_ref())? {
            tracing::info!(task_id = %task.id, "processing approved task");
            let result = thrum_runner::parallel::pipeline::post_approval_pipeline(
                &task_store,
                &gate_store,
                repos_config,
                &event_bus,
                &integration_steps,
                task,
            )
            .await;
            match result {
                Ok(()) => tracing::info!("post-approval pipeline completed"),
                Err(e) => tracing::error!("post-approval pipeline failed: {e:#}"),
            }
            if once {
                break;
            }
            continue;
        }

        // Phase C: Pick next pending task
        let task = match task_store.next_pending(repo_filter.as_ref())? {
            Some(t) => t,
            None => {
                // Phase D: No pending tasks — invoke planner if queue empty
                tracing::info!("no pending tasks, invoking planner");
                let planned = invoke_planner(
                    &task_store,
                    repos_config,
                    agents_dir,
                    &registry,
                    repo_filter.as_ref(),
                )
                .await;
                match planned {
                    Ok(count) if count > 0 => {
                        tracing::info!(count, "planner created new tasks");
                        continue;
                    }
                    Ok(_) => {
                        tracing::info!("planner found nothing to do, loop complete");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("planner failed: {e:#}, loop complete");
                        break;
                    }
                }
            }
        };

        tracing::info!(task_id = %task.id, title = %task.title, repo = %task.repo, "starting task");

        let result = thrum_runner::parallel::pipeline::run_task_pipeline(
            &task_store,
            &gate_store,
            repos_config,
            agents_dir,
            &registry,
            None,
            &event_bus,
            &budget,
            task,
        )
        .await;

        match result {
            Ok(()) => tracing::info!("task pipeline completed successfully"),
            Err(e) => tracing::error!("task pipeline failed: {e:#}"),
        }

        if once {
            break;
        }
    }

    // Persist budget tracker
    {
        let tracker = budget.lock().await;
        let budget_store = BudgetStore::new(db);
        if let Err(e) = budget_store.save(&tracker) {
            tracing::warn!(error = %e, "failed to persist budget tracker");
        }
    }

    Ok(())
}

/// Invoke the planner agent to auto-generate tasks.
async fn invoke_planner(
    task_store: &TaskStore<'_>,
    repos_config: &ReposConfig,
    agents_dir: &Path,
    registry: &BackendRegistry,
    repo_filter: Option<&RepoName>,
) -> Result<usize> {
    let planner = registry
        .chat()
        .or_else(|| registry.agent())
        .context("no backend available for planning")?;

    let planner_prompt_file = agents_dir.join("planner.md");
    let system_prompt = load_agent_prompt(&planner_prompt_file, None)
        .await
        .unwrap_or_default();

    // Build context for the planner
    let mut context = String::new();
    context.push_str("## Current Task Queue\n\n");
    let existing = task_store.list(None, None)?;
    if existing.is_empty() {
        context.push_str("No tasks in queue.\n\n");
    } else {
        for t in &existing {
            context.push_str(&format!(
                "- {} [{}] ({}): {}\n",
                t.id,
                t.status.label(),
                t.repo,
                t.title
            ));
        }
        context.push('\n');
    }

    // Run consistency check for context
    let mut repo_paths: HashMap<RepoName, &std::path::Path> = HashMap::new();
    for repo in &repos_config.repo {
        repo_paths.insert(repo.name.clone(), &repo.path);
    }
    if let Ok(report) = check_consistency(&repo_paths)
        && !report.issues.is_empty()
    {
        context.push_str("## Consistency Issues\n\n");
        for issue in &report.issues {
            context.push_str(&format!("- {issue}\n"));
        }
        context.push('\n');
    }

    if let Some(filter) = repo_filter {
        context.push_str(&format!("\nFocus on repo: {filter}\n"));
    }

    context.push_str("\nGenerate a JSON array of tasks. Respond with ONLY the JSON array.\n");

    let request = AiRequest::new(&context).with_system(system_prompt);
    let result = planner.invoke(&request).await?;

    // Parse JSON array of tasks from planner output
    let tasks: Vec<PlannerTask> = match serde_json::from_str(&result.content) {
        Ok(t) => t,
        Err(_) => {
            // Try to extract JSON from mixed output
            if let Some(start) = result.content.find('[') {
                if let Some(end) = result.content.rfind(']') {
                    serde_json::from_str(&result.content[start..=end]).unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        }
    };

    let mut created = 0;
    for pt in tasks {
        if pt.repo.is_empty() {
            continue;
        }
        let repo_name = RepoName::new(&pt.repo);
        if let Some(filter) = repo_filter
            && &repo_name != filter
        {
            continue;
        }
        let mut task = Task::new(repo_name, pt.title, pt.description);
        task.requirement_id = pt.requirement_id;
        task.acceptance_criteria = pt.acceptance_criteria;
        task_store.insert(task)?;
        created += 1;
    }

    Ok(created)
}

#[derive(serde::Deserialize, Default)]
struct PlannerTask {
    repo: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    requirement_id: Option<String>,
}

// ─── Release Pipeline ───────────────────────────────────────────────────

async fn cmd_release(
    dry_run: bool,
    tag: Option<String>,
    repos_config: &ReposConfig,
    db: &redb::Database,
) -> Result<()> {
    let targets = [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "wasm32-wasip2",
    ];

    if dry_run {
        println!("=== Release Dry Run ===\n");
        if let Some(ref t) = tag {
            println!("Tag: {t}");
        }
        println!("\nWould build for targets:");
        for target in &targets {
            println!("  - {target}");
        }
        println!("\nWould generate artifacts:");
        println!("  - verification-report.json (gate results)");
        println!("  - test-report.json");
        println!("  - traceability-matrix.csv");
        println!("  - checksums.sha256");

        // Show gate status for each repo
        let _gate_store = GateStore::new(db);
        println!("\nLatest gate status:");
        for repo in &repos_config.repo {
            println!("  {}: configured", repo.name);
        }

        println!("\n(dry run complete)");
        return Ok(());
    }

    let tag = tag.context("--tag is required for actual release")?;
    println!("=== Building Release {tag} ===\n");

    let release_dir = PathBuf::from(format!("release/{tag}"));
    std::fs::create_dir_all(&release_dir)?;

    // Step 1: Build for each target
    for repo in &repos_config.repo {
        println!("Building {}...", repo.name);
        for target in &targets {
            let cmd = format!(
                "cargo build --release --target {target} --manifest-path {}",
                repo.path.join("Cargo.toml").display()
            );
            println!("  Target: {target}");
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .current_dir(&repo.path)
                .output()?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(repo = %repo.name, target, "build failed: {stderr}");
                println!("    FAILED (non-fatal, continuing)");
            } else {
                println!("    OK");
            }
        }
    }

    // Step 2: Run all gates and collect reports
    let mut gate_reports = Vec::new();
    for repo in &repos_config.repo {
        println!("\nRunning gates for {}...", repo.name);
        let g1 = run_gate(&GateLevel::Quality, repo)?;
        println!(
            "  Gate 1 (Quality): {}",
            if g1.passed { "PASS" } else { "FAIL" }
        );
        gate_reports.push(serde_json::to_value(&g1)?);

        let g2 = run_gate(&GateLevel::Proof, repo)?;
        println!(
            "  Gate 2 (Proof): {}",
            if g2.passed { "PASS" } else { "FAIL" }
        );
        gate_reports.push(serde_json::to_value(&g2)?);
    }

    // Step 3: Integration gate
    println!("\nRunning Gate 3 (Integration)...");
    let fixture = PathBuf::from("fixtures/pipeline_test.wat");
    let g3 = run_integration_gate(repos_config, &fixture)?;
    println!("  Gate 3: {}", if g3.passed { "PASS" } else { "FAIL" });
    gate_reports.push(serde_json::to_value(&g3)?);

    // Step 4: Write verification report
    let verification = serde_json::json!({
        "tag": tag,
        "timestamp": Utc::now().to_rfc3339(),
        "gates": gate_reports,
        "all_passed": gate_reports.iter().all(|g| g["passed"].as_bool().unwrap_or(false)),
    });
    let report_path = release_dir.join("verification-report.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&verification)?)?;
    println!("\nWrote {}", report_path.display());

    // Step 5: Traceability matrix export
    let trace_store = TraceStore::new(db);
    let task_store = TaskStore::new(db);
    let mut needs_json = NeedsJson::new("Thrum", &tag);
    for task in task_store.list(None, None)? {
        for record in trace_store.get_for_task(task.id.0)? {
            for need in trace_record_to_needs(&record) {
                needs_json.add(need);
            }
        }
    }
    let trace_path = release_dir.join("traceability.json");
    std::fs::write(&trace_path, needs_json.to_json()?)?;
    println!("Wrote {}", trace_path.display());

    // Step 6: Checksums
    let mut checksums = String::new();
    for entry in std::fs::read_dir(&release_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.file_name().is_none_or(|n| n != "checksums.sha256") {
            let data = std::fs::read(&path)?;
            let digest = sha256_hex(&data);
            let name = path.file_name().unwrap().to_string_lossy();
            checksums.push_str(&format!("{digest}  {name}\n"));
        }
    }
    let checksums_path = release_dir.join("checksums.sha256");
    std::fs::write(&checksums_path, &checksums)?;
    println!("Wrote {}", checksums_path.display());

    println!(
        "\nRelease {tag} artifacts ready in {}",
        release_dir.display()
    );
    println!(
        "To publish: gh release create {tag} {}/*",
        release_dir.display()
    );

    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    // Simple SHA-256 using std (no extra dep) — we just need checksums
    use std::io::Write;
    let mut hasher = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("shasum not available");
    hasher.stdin.take().unwrap().write_all(data).ok();
    let output = hasher.wait_with_output().expect("shasum failed");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("?")
        .to_string()
}

// ─── Traces (OTel local viewer) ─────────────────────────────────────────

fn cmd_traces(trace_dir: &Path, action: TracesAction) -> Result<()> {
    let reader = TraceReader::new(trace_dir);

    match action {
        TracesAction::List {
            limit,
            level,
            target,
            filter,
        } => {
            let field_filter = filter.and_then(|f| {
                let parts: Vec<&str> = f.splitn(2, '=').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            });

            let trace_filter = TraceFilter {
                limit: Some(limit),
                level,
                target_prefix: target,
                field_filter,
            };

            let events = reader.read_events(&trace_filter)?;
            if events.is_empty() {
                println!("No trace events found in {}", trace_dir.display());
            } else {
                for event in &events {
                    println!("{event}");
                }
                println!("\n({} events)", events.len());
            }
        }
        TracesAction::Summary => {
            let summary = reader.summary()?;
            println!("=== Trace Storage ===\n");
            print!("{summary}");

            let files = reader.trace_files()?;
            if !files.is_empty() {
                println!("\nFiles:");
                for f in &files {
                    let meta = std::fs::metadata(f)?;
                    println!(
                        "  {} ({:.1} KB)",
                        f.file_name().unwrap_or_default().to_string_lossy(),
                        meta.len() as f64 / 1024.0
                    );
                }
            }
        }
        TracesAction::Dump { limit } => {
            let trace_filter = TraceFilter {
                limit: Some(limit),
                ..Default::default()
            };
            let events = reader.read_events(&trace_filter)?;
            for event in &events {
                println!("{}", serde_json::to_string(event)?);
            }
        }
    }

    Ok(())
}

// ─── Task management ────────────────────────────────────────────────────

fn cmd_task(db: &redb::Database, action: TaskAction) -> Result<()> {
    let store = TaskStore::new(db);

    match action {
        TaskAction::Add {
            repo,
            title,
            desc,
            requirement_id,
            spec,
        } => {
            let repo_name = RepoName::new(repo);
            let mut task = Task::new(repo_name, title, desc);
            task.requirement_id = requirement_id;

            // Load spec from TOML file if provided
            if let Some(spec_path) = spec {
                let content = std::fs::read_to_string(&spec_path)
                    .context(format!("failed to read spec: {}", spec_path.display()))?;
                let parsed_spec = Spec::from_toml(&content)?;
                task.acceptance_criteria = parsed_spec.acceptance_criteria.clone();
                task.spec = Some(parsed_spec);
            }

            let task = store.insert(task)?;
            println!("Created {}: {}", task.id, task.title);
        }
        TaskAction::List { status, repo } => {
            let repo_filter = repo.map(RepoName::new);
            let tasks = store.list(status.as_deref(), repo_filter.as_ref())?;
            if tasks.is_empty() {
                println!("No tasks found.");
            } else {
                println!(
                    "{:<12} {:<18} {:<8} {:<4} TITLE",
                    "ID", "STATUS", "REPO", "TRY"
                );
                println!("{}", "-".repeat(78));
                for t in tasks {
                    println!(
                        "{:<12} {:<18} {:<8} {:<4} {}",
                        t.id,
                        t.status.label(),
                        t.repo,
                        t.retry_count,
                        t.title
                    );
                }
            }
        }
        TaskAction::Approve { id } => {
            let mut task = store
                .get(&TaskId(id))?
                .context(format!("task {id} not found"))?;

            if !task.status.needs_human() {
                anyhow::bail!(
                    "task {} is in state '{}', not awaiting approval",
                    id,
                    task.status.label()
                );
            }

            task.status = TaskStatus::Approved;
            task.updated_at = Utc::now();
            store.update(&task)?;
            println!("Approved {}: {}", task.id, task.title);
        }
        TaskAction::Reject { id, feedback } => {
            let mut task = store
                .get(&TaskId(id))?
                .context(format!("task {id} not found"))?;

            task.status = TaskStatus::Rejected { feedback };
            task.updated_at = Utc::now();
            store.update(&task)?;
            println!("Rejected {}: {}", task.id, task.title);
        }
        TaskAction::Show { id } => {
            let task = store
                .get(&TaskId(id))?
                .context(format!("task {id} not found"))?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TaskAction::SetStatus { id, status } => {
            let mut task = store
                .get(&TaskId(id))?
                .context(format!("task {id} not found"))?;

            task.status = match status.as_str() {
                "pending" => TaskStatus::Pending,
                "merged" => TaskStatus::Merged {
                    commit_sha: "manually-set".into(),
                },
                "approved" => TaskStatus::Approved,
                other => {
                    anyhow::bail!("unsupported status '{other}'. Use: pending, approved, merged")
                }
            };
            task.updated_at = Utc::now();
            store.update(&task)?;
            println!(
                "Set {} to '{}': {}",
                task.id,
                task.status.label(),
                task.title
            );
        }
    }

    Ok(())
}

fn cmd_status(db: &redb::Database, config_path: &Path) -> Result<()> {
    let store = TaskStore::new(db);
    let counts = store.status_counts()?;

    println!("=== Automator Status ===\n");
    println!("Task counts:");
    for (status, count) in &counts {
        println!("  {status:<20} {count}");
    }
    let total: usize = counts.values().sum();
    println!("  {:<20} {total}", "total");

    if config_path.exists() {
        println!("\nConsistency: run `thrum check` for full report");
    }

    Ok(())
}

fn cmd_check(config_path: &Path) -> Result<()> {
    let repos_config = ReposConfig::load(config_path)?;
    let mut repo_paths: HashMap<RepoName, &std::path::Path> = HashMap::new();
    for repo in &repos_config.repo {
        repo_paths.insert(repo.name.clone(), &repo.path);
    }

    let report = check_consistency(&repo_paths)?;

    println!("=== Consistency Report ===\n");
    println!("wasmparser versions:");
    for (repo, ver) in &report.wasmparser_versions {
        println!("  {repo}: {ver}");
    }
    println!("\nRust editions:");
    for (repo, ed) in &report.rust_editions {
        println!("  {repo}: {ed}");
    }

    if report.issues.is_empty() {
        println!("\nNo issues found.");
    } else {
        println!("\nIssues ({}):", report.issues.len());
        for issue in &report.issues {
            println!("  - {issue}");
        }
    }

    Ok(())
}

fn cmd_trace(db: &redb::Database, action: TraceAction) -> Result<()> {
    let trace_store = TraceStore::new(db);

    match action {
        TraceAction::Export { output, version } => {
            let mut needs_json = NeedsJson::new("Thrum", &version);
            let task_store = TaskStore::new(db);
            for task in task_store.list(None, None)? {
                for record in trace_store.get_for_task(task.id.0)? {
                    for need in trace_record_to_needs(&record) {
                        needs_json.add(need);
                    }
                }
            }
            let json = needs_json.to_json()?;
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&output, &json)?;
            println!(
                "Exported {} needs to {}",
                needs_json.needs.len(),
                output.display()
            );
        }
        TraceAction::Rst { tool, output } => {
            let rst = thrum_core::sphinx_needs::generate_traceability_rst(&tool);
            let out_path =
                output.unwrap_or_else(|| PathBuf::from(format!("docs/traceability/{tool}.rst")));
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&out_path, &rst)?;
            println!("Generated RST at {}", out_path.display());
        }
        TraceAction::Gaps => {
            let task_store = TaskStore::new(db);
            println!("=== Traceability Gaps ===\n");
            let mut has_gaps = false;
            for task in task_store.list(None, None)? {
                if let Some(ref req_id) = task.requirement_id {
                    let records = trace_store.get_for_task(task.id.0)?;
                    let has_test = records.iter().any(|r| {
                        matches!(
                            r.artifact,
                            thrum_core::traceability::TraceArtifact::Test { .. }
                        )
                    });
                    let has_proof = records.iter().any(|r| {
                        matches!(
                            r.artifact,
                            thrum_core::traceability::TraceArtifact::Proof { .. }
                        )
                    });
                    let has_review = records.iter().any(|r| {
                        matches!(
                            r.artifact,
                            thrum_core::traceability::TraceArtifact::Review { .. }
                        )
                    });

                    let mut gaps = Vec::new();
                    if !has_test {
                        gaps.push("test");
                    }
                    if !has_proof {
                        gaps.push("proof");
                    }
                    if !has_review {
                        gaps.push("review");
                    }

                    if !gaps.is_empty() {
                        has_gaps = true;
                        println!("  {} ({}): missing {}", req_id, task.repo, gaps.join(", "));
                    }
                }
            }
            if !has_gaps {
                println!("  No gaps found.");
            }
        }
    }
    Ok(())
}

fn cmd_safety() -> Result<()> {
    use thrum_core::safety::*;

    println!("=== Tool Safety Classification ===\n");
    println!(
        "{:<8} {:<5} {:<5} {:<6} {:<10} QUALIFICATION",
        "TOOL", "TI", "TD", "TCL", "ASIL"
    );
    println!("{}", "-".repeat(70));

    let tools = [
        ("loom", ToolImpact::Ti2, ToolDetection::Td2, "ASIL B"),
        ("synth", ToolImpact::Ti2, ToolDetection::Td3, "ASIL D"),
        ("meld", ToolImpact::Ti2, ToolDetection::Td2, "QM"),
    ];

    for (name, ti, td, asil) in &tools {
        let tcl = determine_tcl(*ti, *td);
        let methods = qualification_methods(tcl);
        let method_str = if methods.is_empty() {
            "None required".to_string()
        } else {
            format!("{} methods", methods.len())
        };
        println!(
            "{:<8} {:<5} {:<5} {:<6} {:<10} {}",
            name,
            format!("{ti:?}"),
            format!("{td:?}"),
            tcl,
            asil,
            method_str,
        );
    }

    println!("\n=== Qualification Methods ===\n");
    for (name, ti, td, _) in &tools {
        let tcl = determine_tcl(*ti, *td);
        let methods = qualification_methods(tcl);
        if !methods.is_empty() {
            println!("{name} ({tcl}):");
            for m in methods {
                println!("  - {m}");
            }
            println!();
        }
    }

    println!("=== ASPICE Process Mapping ===\n");
    for (stage, process) in pipeline_aspice_mapping() {
        println!("  {stage:<40} → {process}");
    }

    Ok(())
}
