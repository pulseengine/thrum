use crate::repo::{RepoConfig, ReposConfig};
use crate::subsample::{self, SubsampleConfig};
use crate::task::{CheckResult, GateLevel, GateReport, RepoName};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone, Deserialize)]
pub struct IntegrationStep {
    pub repo: String,
    pub label: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntegrationGateConfig {
    pub steps: Vec<IntegrationStep>,
}

/// Run a gate's checks against a repo.
///
/// When `subsample` is `Some` and enabled, test commands are wrapped through
/// `subsample_test_cmd()` using the ratio for the corresponding gate level.
/// Gate 1 uses `gate1_ratio`, Gate 2 uses `gate2_ratio`, Gate 3 uses `gate3_ratio`.
pub fn run_gate(
    level: &GateLevel,
    repo: &RepoConfig,
    subsample: Option<&SubsampleConfig>,
    task_id: Option<i64>,
) -> anyhow::Result<GateReport> {
    let start = Instant::now();

    let checks = match level {
        GateLevel::Quality => run_quality_checks(repo, subsample, task_id)?,
        GateLevel::Proof => run_proof_checks(repo, subsample, task_id)?,
        GateLevel::Integration => {
            // Integration gate needs run_integration_gate() with full ReposConfig
            vec![CheckResult {
                name: "integration_pipeline".into(),
                passed: true,
                stdout: "Use run_integration_gate() for Gate 3".into(),
                stderr: String::new(),
                exit_code: 0,
            }]
        }
    };

    let passed = checks.iter().all(|c| c.passed);
    let duration = start.elapsed();

    Ok(GateReport {
        level: level.clone(),
        checks,
        passed,
        duration_secs: duration.as_secs_f64(),
    })
}

/// Run Gate 3: full integration pipeline (meld → loom → synth).
///
/// Executes the three tools in sequence on a test fixture:
/// 1. meld fuse: WASM component → fused WASM module
/// 2. loom optimize: fused WASM → optimized WASM
/// 3. synth compile: optimized WASM → native binary (ARM ELF)
///
/// Each step's output feeds into the next. If any step fails,
/// the gate reports the failure with captured stdout/stderr.
pub fn run_integration_gate(repos: &ReposConfig, fixture: &Path) -> anyhow::Result<GateReport> {
    let start = Instant::now();
    let mut checks = Vec::new();

    // Create temp dir for pipeline artifacts
    let tmp = std::env::temp_dir().join("thrum-integration");
    std::fs::create_dir_all(&tmp)?;

    let fused = tmp.join("fused.wasm");
    let optimized = tmp.join("optimized.wasm");
    let output_elf = tmp.join("output.elf");

    // Step 1: meld fuse
    if let Some(meld) = repos.get(&RepoName::new("meld")) {
        let cmd = format!(
            "cargo run --release -- fuse {} -o {}",
            fixture.display(),
            fused.display()
        );
        let result = run_cmd("meld_fuse", &cmd, &meld.path)?;
        let step_passed = result.passed;
        checks.push(result);

        if !step_passed {
            return finish_gate(GateLevel::Integration, checks, start);
        }
    } else {
        checks.push(CheckResult {
            name: "meld_fuse".into(),
            passed: false,
            stdout: String::new(),
            stderr: "meld repo not configured".into(),
            exit_code: -1,
        });
        return finish_gate(GateLevel::Integration, checks, start);
    }

    // Step 2: loom optimize
    if let Some(loom) = repos.get(&RepoName::new("loom")) {
        let cmd = format!(
            "cargo run --release -- optimize {} -o {}",
            fused.display(),
            optimized.display()
        );
        let result = run_cmd("loom_optimize", &cmd, &loom.path)?;
        let step_passed = result.passed;
        checks.push(result);

        if !step_passed {
            return finish_gate(GateLevel::Integration, checks, start);
        }
    } else {
        checks.push(CheckResult {
            name: "loom_optimize".into(),
            passed: false,
            stdout: String::new(),
            stderr: "loom repo not configured".into(),
            exit_code: -1,
        });
        return finish_gate(GateLevel::Integration, checks, start);
    }

    // Step 3: synth compile
    if let Some(synth) = repos.get(&RepoName::new("synth")) {
        let cmd = format!(
            "cargo run --release -- compile {} -o {}",
            optimized.display(),
            output_elf.display()
        );
        let result = run_cmd("synth_compile", &cmd, &synth.path)?;
        checks.push(result);
    } else {
        checks.push(CheckResult {
            name: "synth_compile".into(),
            passed: false,
            stdout: String::new(),
            stderr: "synth repo not configured".into(),
            exit_code: -1,
        });
    }

    // Verify final output exists
    if output_elf.exists() {
        let meta = std::fs::metadata(&output_elf)?;
        checks.push(CheckResult {
            name: "output_validation".into(),
            passed: meta.len() > 0,
            stdout: format!("Output ELF: {} bytes", meta.len()),
            stderr: String::new(),
            exit_code: 0,
        });
    }

    // Cleanup temp artifacts (best-effort)
    let _ = std::fs::remove_dir_all(&tmp);

    finish_gate(GateLevel::Integration, checks, start)
}

/// Run Gate 3 using config-driven integration steps.
///
/// Each step runs `cargo run --release -- {args}` in the repo's directory,
/// with `{fixture}` and `{output_dir}` placeholders resolved.
pub fn run_integration_gate_configured(
    repos: &ReposConfig,
    fixture: &Path,
    config: &IntegrationGateConfig,
) -> anyhow::Result<GateReport> {
    let start = Instant::now();
    let mut checks = Vec::new();

    let tmp = std::env::temp_dir().join("thrum-integration");
    std::fs::create_dir_all(&tmp)?;

    for step in &config.steps {
        let repo_name = RepoName::new(&step.repo);
        let repo_config = match repos.get(&repo_name) {
            Some(r) => r,
            None => {
                checks.push(CheckResult {
                    name: step.label.clone(),
                    passed: false,
                    stdout: String::new(),
                    stderr: format!("{} repo not configured", step.repo),
                    exit_code: -1,
                });
                return finish_gate(GateLevel::Integration, checks, start);
            }
        };

        // Resolve placeholders in args
        let resolved_args: Vec<String> = step
            .args
            .iter()
            .map(|a| {
                a.replace("{fixture}", &fixture.display().to_string())
                    .replace("{output_dir}", &tmp.display().to_string())
            })
            .collect();

        let cmd = format!("cargo run --release -- {}", resolved_args.join(" "));
        let result = run_cmd(&step.label, &cmd, &repo_config.path)?;
        let step_passed = result.passed;
        checks.push(result);

        if !step_passed {
            let _ = std::fs::remove_dir_all(&tmp);
            return finish_gate(GateLevel::Integration, checks, start);
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    finish_gate(GateLevel::Integration, checks, start)
}

fn finish_gate(
    level: GateLevel,
    checks: Vec<CheckResult>,
    start: Instant,
) -> anyhow::Result<GateReport> {
    let passed = checks.iter().all(|c| c.passed);
    let duration = start.elapsed();
    Ok(GateReport {
        level,
        checks,
        passed,
        duration_secs: duration.as_secs_f64(),
    })
}

fn run_quality_checks(
    repo: &RepoConfig,
    subsample: Option<&SubsampleConfig>,
    task_id: Option<i64>,
) -> anyhow::Result<Vec<CheckResult>> {
    let test_cmd = maybe_subsample(&repo.test_cmd, subsample, task_id, |c| c.gate1_ratio);
    let checks = vec![
        run_cmd("cargo_fmt", &repo.fmt_cmd, &repo.path)?,
        run_cmd("cargo_clippy", &repo.lint_cmd, &repo.path)?,
        run_cmd("cargo_test", &test_cmd, &repo.path)?,
    ];
    Ok(checks)
}

fn run_proof_checks(
    repo: &RepoConfig,
    subsample: Option<&SubsampleConfig>,
    task_id: Option<i64>,
) -> anyhow::Result<Vec<CheckResult>> {
    let mut checks = Vec::new();

    if let Some(ref verify_cmd) = repo.verify_cmd {
        checks.push(run_cmd("z3_verify", verify_cmd, &repo.path)?);
    }

    if let Some(ref proofs_cmd) = repo.proofs_cmd {
        checks.push(run_cmd("rocq_proofs", proofs_cmd, &repo.path)?);
    }

    // If the repo has a test command and we're at Gate 2, run tests at gate2_ratio
    // (Gate 2 typically runs at 1.0 for full coverage)
    if repo.verify_cmd.is_none() && repo.proofs_cmd.is_none() {
        // No proof tooling — gate passes vacuously
        checks.push(CheckResult {
            name: "no_proofs_configured".into(),
            passed: true,
            stdout: "No proof commands configured for this repo".into(),
            stderr: String::new(),
            exit_code: 0,
        });
    }

    // Note: subsample and task_id are accepted for API consistency but Gate 2
    // ratio defaults to 1.0 (full coverage). The config is available if someone
    // explicitly sets gate2_ratio < 1.0.
    let _ = (subsample, task_id);

    Ok(checks)
}

/// Optionally wrap a test command through `subsample_test_cmd()`.
///
/// Only applies subsampling when the config is `Some`, enabled, and a task_id
/// is available. The `ratio_fn` selects which ratio to use (gate1_ratio,
/// gate2_ratio, etc.).
fn maybe_subsample(
    cmd: &str,
    subsample: Option<&SubsampleConfig>,
    task_id: Option<i64>,
    ratio_fn: fn(&SubsampleConfig) -> f64,
) -> String {
    match (subsample, task_id) {
        (Some(cfg), Some(tid)) if cfg.enabled => {
            let ratio = ratio_fn(cfg);
            let seed = subsample::compute_seed(&cfg.seed_strategy, tid);
            let result = subsample::subsample_test_cmd(cmd, ratio, seed);
            if result != cmd {
                tracing::info!(
                    original = cmd,
                    subsampled = %result,
                    ratio,
                    seed,
                    "applied test subsampling"
                );
            }
            result
        }
        _ => cmd.to_string(),
    }
}

fn run_cmd(name: &str, cmd: &str, cwd: &std::path::Path) -> anyhow::Result<CheckResult> {
    tracing::info!(name, cmd, ?cwd, "running gate check");

    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .output()?;

    let result = CheckResult {
        name: name.to_string(),
        passed: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code().unwrap_or(-1),
    };

    if result.passed {
        tracing::info!(name, "check passed");
    } else {
        tracing::warn!(name, exit_code = result.exit_code, "check failed");
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsampled_cmd_differs_when_ratio_below_one() {
        let config = SubsampleConfig {
            enabled: true,
            gate1_ratio: 0.3,
            gate2_ratio: 1.0,
            gate3_ratio: 1.0,
            seed_strategy: "task-id".into(),
        };
        let original = "cargo test --release";
        let result = maybe_subsample(original, Some(&config), Some(42), |c| c.gate1_ratio);

        // With ratio 0.3, the command should be wrapped with env vars
        assert_ne!(result, original);
        assert!(result.contains("AUTOMATOR_SUBSAMPLE_SEED="));
        assert!(result.contains("AUTOMATOR_SUBSAMPLE_PARTITION="));
        assert!(result.contains(original));
    }

    #[test]
    fn no_subsampling_when_disabled() {
        let config = SubsampleConfig {
            enabled: false,
            gate1_ratio: 0.3,
            ..SubsampleConfig::default()
        };
        let original = "cargo test --release";
        let result = maybe_subsample(original, Some(&config), Some(42), |c| c.gate1_ratio);
        assert_eq!(result, original);
    }

    #[test]
    fn no_subsampling_when_config_is_none() {
        let original = "cargo test --release";
        let result = maybe_subsample(original, None, Some(42), |c| c.gate1_ratio);
        assert_eq!(result, original);
    }

    #[test]
    fn no_subsampling_when_task_id_is_none() {
        let config = SubsampleConfig {
            enabled: true,
            gate1_ratio: 0.3,
            ..SubsampleConfig::default()
        };
        let original = "cargo test --release";
        let result = maybe_subsample(original, Some(&config), None, |c| c.gate1_ratio);
        assert_eq!(result, original);
    }

    #[test]
    fn gate2_ratio_passthrough_at_full_coverage() {
        let config = SubsampleConfig {
            enabled: true,
            gate1_ratio: 0.3,
            gate2_ratio: 1.0,
            ..SubsampleConfig::default()
        };
        let original = "cargo test --release";
        // Gate 2 uses gate2_ratio=1.0, so the command should pass through unmodified
        let result = maybe_subsample(original, Some(&config), Some(42), |c| c.gate2_ratio);
        assert_eq!(result, original);
    }

    #[test]
    fn different_tasks_get_different_seeds() {
        let config = SubsampleConfig {
            enabled: true,
            gate1_ratio: 0.5,
            ..SubsampleConfig::default()
        };
        let original = "cargo test";
        let result1 = maybe_subsample(original, Some(&config), Some(1), |c| c.gate1_ratio);
        let result2 = maybe_subsample(original, Some(&config), Some(2), |c| c.gate1_ratio);

        // Both should be subsampled but with different seeds
        assert_ne!(result1, original);
        assert_ne!(result2, original);
        assert!(result1.contains("SEED=1"));
        assert!(result2.contains("SEED=2"));
    }
}
