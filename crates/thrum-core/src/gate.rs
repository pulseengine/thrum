use crate::repo::{RepoConfig, ReposConfig};
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
pub fn run_gate(level: &GateLevel, repo: &RepoConfig) -> anyhow::Result<GateReport> {
    let start = Instant::now();

    let checks = match level {
        GateLevel::Quality => run_quality_checks(repo)?,
        GateLevel::Proof => run_proof_checks(repo)?,
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

fn run_quality_checks(repo: &RepoConfig) -> anyhow::Result<Vec<CheckResult>> {
    let checks = vec![
        run_cmd("cargo_fmt", &repo.fmt_cmd, &repo.path)?,
        run_cmd("cargo_clippy", &repo.lint_cmd, &repo.path)?,
        run_cmd("cargo_test", &repo.test_cmd, &repo.path)?,
    ];
    Ok(checks)
}

fn run_proof_checks(repo: &RepoConfig) -> anyhow::Result<Vec<CheckResult>> {
    let mut checks = Vec::new();

    if let Some(ref verify_cmd) = repo.verify_cmd {
        checks.push(run_cmd("z3_verify", verify_cmd, &repo.path)?);
    }

    if let Some(ref proofs_cmd) = repo.proofs_cmd {
        checks.push(run_cmd("rocq_proofs", proofs_cmd, &repo.path)?);
    }

    // If no proof tooling configured, gate passes vacuously
    if checks.is_empty() {
        checks.push(CheckResult {
            name: "no_proofs_configured".into(),
            passed: true,
            stdout: "No proof commands configured for this repo".into(),
            stderr: String::new(),
            exit_code: 0,
        });
    }

    Ok(checks)
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
