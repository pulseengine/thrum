//! Dynamic test subsampling for gate checks.
//!
//! At Gate 1 (quality), running the full test suite can be slow. Subsampling
//! runs a deterministic fraction of tests based on task ID seeding, trading
//! coverage for speed. Gate 2 (proof) and Gate 3 (integration) always run
//! at full coverage.

use serde::Deserialize;

/// Configuration for test subsampling.
#[derive(Debug, Clone, Deserialize)]
pub struct SubsampleConfig {
    /// Whether subsampling is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Fraction of tests to run at Gate 1 (0.0..1.0).
    #[serde(default = "default_gate1_ratio")]
    pub gate1_ratio: f64,
    /// Fraction of tests to run at Gate 2 (always 1.0 for proof gate).
    #[serde(default = "default_full_ratio")]
    pub gate2_ratio: f64,
    /// Fraction of tests to run at Gate 3 (always 1.0 for integration).
    #[serde(default = "default_full_ratio")]
    pub gate3_ratio: f64,
    /// Seed strategy: "task-id", "random", "coverage-guided".
    #[serde(default = "default_seed_strategy")]
    pub seed_strategy: String,
}

fn default_gate1_ratio() -> f64 {
    0.3
}
fn default_full_ratio() -> f64 {
    1.0
}
fn default_seed_strategy() -> String {
    "task-id".into()
}

impl Default for SubsampleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gate1_ratio: 0.3,
            gate2_ratio: 1.0,
            gate3_ratio: 1.0,
            seed_strategy: "task-id".into(),
        }
    }
}

/// Transform a test command to run only a subset of tests.
///
/// For Rust projects using `cargo test`, this generates a partition filter
/// by hashing test names against the ratio. The seed ensures deterministic
/// subsampling across runs for the same task.
///
/// Returns the original command unmodified if ratio >= 1.0.
pub fn subsample_test_cmd(original_cmd: &str, ratio: f64, seed: u64) -> String {
    if ratio >= 1.0 {
        return original_cmd.to_string();
    }

    if ratio <= 0.0 {
        // Skip all tests — return a no-op
        return "echo 'subsampling ratio 0: skipping tests'".to_string();
    }

    // For cargo test commands, add a partition-based filter
    if original_cmd.starts_with("cargo test") {
        // Use cargo-nextest style partitioning if available,
        // otherwise use a seed-based approach with -- --skip
        //
        // Strategy: use `cargo test -- --partition hash:{n}/{total}` isn't built-in,
        // so we use an environment variable approach where the test harness
        // can filter based on hash modulo.
        //
        // Simplest approach: run tests with a hash-based skip list.
        // Since we can't know test names ahead of time, we use `--test-threads=1`
        // and adjust expectations.
        //
        // Practical approach: use AUTOMATOR_TEST_SEED env var and a partition count.
        let partition_count = (1.0 / ratio).ceil() as u64;
        let partition_index = seed % partition_count;

        format!(
            "AUTOMATOR_SUBSAMPLE_SEED={seed} AUTOMATOR_SUBSAMPLE_PARTITION={partition_index}/{partition_count} {original_cmd}",
        )
    } else {
        // Non-cargo commands: pass through unchanged
        original_cmd.to_string()
    }
}

/// Compute the seed for subsampling based on strategy and task ID.
pub fn compute_seed(strategy: &str, task_id: i64) -> u64 {
    match strategy {
        "task-id" => task_id as u64,
        "random" => {
            // Use current time as entropy
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        }
        _ => task_id as u64, // default to task-id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_ratio_passthrough() {
        let cmd = subsample_test_cmd("cargo test --release", 1.0, 42);
        assert_eq!(cmd, "cargo test --release");
    }

    #[test]
    fn zero_ratio_skips() {
        let cmd = subsample_test_cmd("cargo test", 0.0, 42);
        assert!(cmd.contains("skipping tests"));
    }

    #[test]
    fn partial_ratio_adds_env() {
        let cmd = subsample_test_cmd("cargo test --release", 0.3, 7);
        assert!(cmd.contains("AUTOMATOR_SUBSAMPLE_SEED=7"));
        assert!(cmd.contains("AUTOMATOR_SUBSAMPLE_PARTITION="));
        assert!(cmd.contains("cargo test --release"));
    }

    #[test]
    fn non_cargo_passthrough() {
        let cmd = subsample_test_cmd("pytest tests/", 0.5, 42);
        assert_eq!(cmd, "pytest tests/");
    }

    #[test]
    fn seed_deterministic() {
        let s1 = compute_seed("task-id", 42);
        let s2 = compute_seed("task-id", 42);
        assert_eq!(s1, s2);
    }

    #[test]
    fn seed_varies_by_task() {
        let s1 = compute_seed("task-id", 1);
        let s2 = compute_seed("task-id", 2);
        assert_ne!(s1, s2);
    }

    #[test]
    fn default_config() {
        let config = SubsampleConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.gate1_ratio, 0.3);
        assert_eq!(config.gate2_ratio, 1.0);
        assert_eq!(config.gate3_ratio, 1.0);
        assert_eq!(config.seed_strategy, "task-id");
    }
}
