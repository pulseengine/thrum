//! Convergence-aware retry with strategy rotation.
//!
//! Instead of blind retries, tracks failure signatures (error type + check name)
//! and rotates strategy when the same failure repeats:
//!
//! 1. First occurrence: normal retry with failure feedback
//! 2. Same signature repeats: retry with expanded context (more stderr, related files)
//! 3. Third repeat: retry with a different approach prompt
//! 4. Fourth+ repeat: flag for human review — agent is stuck
//!
//! Inspired by EvoAgentX convergence-aware retries.

use crate::task::{GateLevel, GateReport, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// How a failure signature is identified.
///
/// Two failures share a signature when they come from the same gate level,
/// the same check name, and have overlapping error content. The `error_hash`
/// captures the first 200 chars of stderr — enough to detect the same
/// compilation error or test failure without being sensitive to line numbers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FailureSignature {
    /// Which gate failed (Quality, Proof, Integration).
    pub gate_level: GateLevel,
    /// Which check within the gate (e.g. "cargo_fmt", "cargo_clippy", "cargo_test").
    pub check_name: String,
    /// Normalized hash of the error content for similarity detection.
    pub error_hash: String,
}

impl FailureSignature {
    /// Extract failure signatures from a gate report.
    ///
    /// Returns one signature per failed check in the report.
    pub fn from_gate_report(report: &GateReport) -> Vec<Self> {
        report
            .checks
            .iter()
            .filter(|c| !c.passed)
            .map(|c| Self {
                gate_level: report.level.clone(),
                check_name: c.name.clone(),
                error_hash: normalize_error(&c.stderr),
            })
            .collect()
    }
}

impl std::fmt::Display for FailureSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            self.gate_level,
            self.check_name,
            &self.error_hash[..self.error_hash.len().min(8)]
        )
    }
}

/// A recorded failure event for convergence tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub task_id: TaskId,
    pub signature: FailureSignature,
    /// How many times this exact signature has been seen for this task.
    pub occurrence_count: u32,
    /// The full stderr from the most recent occurrence.
    pub latest_stderr: String,
    /// When this failure was first observed.
    pub first_seen: DateTime<Utc>,
    /// When this failure was most recently observed.
    pub last_seen: DateTime<Utc>,
}

impl FailureRecord {
    pub fn new(task_id: TaskId, signature: FailureSignature, stderr: String) -> Self {
        let now = Utc::now();
        Self {
            task_id,
            signature,
            occurrence_count: 1,
            latest_stderr: stderr,
            first_seen: now,
            last_seen: now,
        }
    }

    /// Record another occurrence of the same failure.
    pub fn record_occurrence(&mut self, stderr: String) {
        self.occurrence_count += 1;
        self.latest_stderr = stderr;
        self.last_seen = Utc::now();
    }
}

/// The retry strategy to use, escalating with repeated failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetryStrategy {
    /// First failure: normal retry with the standard failure feedback.
    Normal,
    /// Same failure seen twice: provide expanded context — full stderr,
    /// file patterns involved, and explicit instructions to try something different.
    ExpandedContext,
    /// Same failure seen three times: suggest a fundamentally different approach.
    /// The prompt includes an explicit "do NOT repeat the same fix" directive.
    DifferentApproach,
    /// Same failure seen four+ times: the agent is stuck. Flag for human review.
    HumanReview,
}

impl RetryStrategy {
    /// Determine strategy based on how many times the same failure has occurred.
    pub fn from_occurrence_count(count: u32) -> Self {
        match count {
            0 | 1 => RetryStrategy::Normal,
            2 => RetryStrategy::ExpandedContext,
            3 => RetryStrategy::DifferentApproach,
            _ => RetryStrategy::HumanReview,
        }
    }

    /// Short label for display and logging.
    pub fn label(&self) -> &'static str {
        match self {
            RetryStrategy::Normal => "normal",
            RetryStrategy::ExpandedContext => "expanded-context",
            RetryStrategy::DifferentApproach => "different-approach",
            RetryStrategy::HumanReview => "human-review",
        }
    }

    /// Whether this strategy requires human intervention (stops automatic retry).
    pub fn needs_human(&self) -> bool {
        matches!(self, RetryStrategy::HumanReview)
    }

    /// Generate the strategy-specific prompt augmentation.
    ///
    /// This is appended to the task description alongside the failure feedback
    /// to guide the agent's retry behavior.
    pub fn prompt_augmentation(&self, signatures: &[FailureSignature]) -> String {
        let sig_list: String = signatures
            .iter()
            .map(|s| format!("  - {}: {}", s.check_name, s.gate_level))
            .collect::<Vec<_>>()
            .join("\n");

        match self {
            RetryStrategy::Normal => String::new(),
            RetryStrategy::ExpandedContext => {
                format!(
                    "\n\n## ⚠️ Convergence Warning — Repeated Failure Detected\n\
                     The following checks have failed with similar errors on a previous attempt:\n\
                     {sig_list}\n\n\
                     **Strategy: Expanded Context** — Read the FULL error output carefully. \
                     The previous fix attempt did not resolve the root cause. \
                     Focus on understanding WHY the error persists rather than applying \
                     superficial fixes. Check related files and upstream dependencies."
                )
            }
            RetryStrategy::DifferentApproach => {
                format!(
                    "\n\n## 🔄 Convergence Alert — Strategy Rotation Required\n\
                     The following checks have failed with the SAME errors across multiple attempts:\n\
                     {sig_list}\n\n\
                     **Strategy: Different Approach** — Your previous approach is NOT working. \
                     Do NOT repeat the same fix. Instead:\n\
                     1. Reconsider the fundamental approach to this task\n\
                     2. Look for alternative implementations or workarounds\n\
                     3. Check if the acceptance criteria can be met differently\n\
                     4. Consider if a dependency or upstream change is needed\n\n\
                     The definition of insanity is doing the same thing and expecting different results."
                )
            }
            RetryStrategy::HumanReview => {
                format!(
                    "\n\n## 🛑 Convergence Detected — Human Review Required\n\
                     The following checks have repeatedly failed with the same errors:\n\
                     {sig_list}\n\n\
                     This task has been flagged for human review. \
                     The automated agent has been unable to resolve these issues \
                     after multiple strategy rotations."
                )
            }
        }
    }
}

/// Convergence analysis result for a task's retry attempt.
#[derive(Debug, Clone)]
pub struct ConvergenceAnalysis {
    /// The recommended retry strategy (based on the worst-case signature).
    pub strategy: RetryStrategy,
    /// Signatures that were detected as repeated failures.
    pub repeated_signatures: Vec<FailureSignature>,
    /// Per-signature occurrence counts.
    pub occurrence_counts: HashMap<String, u32>,
}

impl ConvergenceAnalysis {
    /// Build a convergence analysis from existing failure records and a new gate report.
    ///
    /// Compares the new report's failure signatures against historical records
    /// to determine how many times each signature has been seen.
    pub fn analyze(existing_records: &[FailureRecord], new_report: &GateReport) -> Self {
        let new_signatures = FailureSignature::from_gate_report(new_report);
        let mut repeated = Vec::new();
        let mut counts = HashMap::new();
        let mut worst_count: u32 = 0;

        for sig in &new_signatures {
            // Find matching historical record
            let historical_count = existing_records
                .iter()
                .filter(|r| r.signature == *sig)
                .map(|r| r.occurrence_count)
                .max()
                .unwrap_or(0);

            // The new occurrence makes it historical_count + 1
            let total = historical_count + 1;
            counts.insert(sig.to_string(), total);

            if historical_count > 0 {
                repeated.push(sig.clone());
            }
            if total > worst_count {
                worst_count = total;
            }
        }

        let strategy = if repeated.is_empty() {
            RetryStrategy::Normal
        } else {
            RetryStrategy::from_occurrence_count(worst_count)
        };

        Self {
            strategy,
            repeated_signatures: repeated,
            occurrence_counts: counts,
        }
    }
}

/// Normalize error output for signature comparison.
///
/// Strips line numbers, timestamps, and whitespace variations so that
/// the "same" error produces the same hash even if file positions shift.
fn normalize_error(stderr: &str) -> String {
    // Take first 300 chars to focus on the error type, not the full trace
    let truncated: String = stderr.chars().take(300).collect();

    // Remove line numbers (e.g. "src/lib.rs:42:5" -> "src/lib.rs::")
    let normalized = truncated
        .lines()
        .map(|line| {
            // Strip ANSI escape codes
            let clean = strip_ansi(line);
            // Normalize paths with line numbers: file.rs:123:45 -> file.rs
            normalize_line_numbers(&clean)
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Hash the normalized content
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for byte in normalized.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    format!("{hash:016x}")
}

/// Strip ANSI escape codes from a string.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until we find a letter (end of escape sequence)
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Normalize line/column numbers in file paths.
///
/// Converts patterns like "src/lib.rs:42:5" to "src/lib.rs" so that
/// the same error at different line numbers still matches.
fn normalize_line_numbers(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        result.push(c);
        // After ".rs:" or similar extension, skip digits and colons
        if c == ':' && result.len() > 4 {
            let before = &result[result.len().saturating_sub(5)..result.len() - 1];
            if before.ends_with(".rs") || before.ends_with(".toml") {
                // Skip the line:col numbers
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_digit() || next == ':' {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{CheckResult, GateLevel, GateReport};

    fn make_gate_report(checks: Vec<(&str, bool, &str)>) -> GateReport {
        GateReport {
            level: GateLevel::Quality,
            checks: checks
                .into_iter()
                .map(|(name, passed, stderr)| CheckResult {
                    name: name.to_string(),
                    passed,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                    exit_code: if passed { 0 } else { 1 },
                })
                .collect(),
            passed: false,
            duration_secs: 1.0,
        }
    }

    #[test]
    fn failure_signature_from_report() {
        let report = make_gate_report(vec![
            ("cargo_fmt", true, ""),
            ("cargo_clippy", false, "error: unused variable"),
            ("cargo_test", false, "test failed"),
        ]);
        let sigs = FailureSignature::from_gate_report(&report);
        assert_eq!(sigs.len(), 2);
        assert_eq!(sigs[0].check_name, "cargo_clippy");
        assert_eq!(sigs[1].check_name, "cargo_test");
    }

    #[test]
    fn strategy_escalation() {
        assert_eq!(
            RetryStrategy::from_occurrence_count(1),
            RetryStrategy::Normal
        );
        assert_eq!(
            RetryStrategy::from_occurrence_count(2),
            RetryStrategy::ExpandedContext
        );
        assert_eq!(
            RetryStrategy::from_occurrence_count(3),
            RetryStrategy::DifferentApproach
        );
        assert_eq!(
            RetryStrategy::from_occurrence_count(4),
            RetryStrategy::HumanReview
        );
        assert_eq!(
            RetryStrategy::from_occurrence_count(10),
            RetryStrategy::HumanReview
        );
    }

    #[test]
    fn strategy_needs_human() {
        assert!(!RetryStrategy::Normal.needs_human());
        assert!(!RetryStrategy::ExpandedContext.needs_human());
        assert!(!RetryStrategy::DifferentApproach.needs_human());
        assert!(RetryStrategy::HumanReview.needs_human());
    }

    #[test]
    fn convergence_analysis_no_history() {
        let report = make_gate_report(vec![("cargo_test", false, "test failed")]);
        let analysis = ConvergenceAnalysis::analyze(&[], &report);
        assert_eq!(analysis.strategy, RetryStrategy::Normal);
        assert!(analysis.repeated_signatures.is_empty());
    }

    #[test]
    fn convergence_analysis_with_repeated_failure() {
        let report = make_gate_report(vec![("cargo_test", false, "test failed: assertion")]);
        let sigs = FailureSignature::from_gate_report(&report);

        // Simulate existing record with same signature
        let existing = vec![FailureRecord {
            task_id: TaskId(1),
            signature: sigs[0].clone(),
            occurrence_count: 1,
            latest_stderr: "test failed: assertion".into(),
            first_seen: Utc::now(),
            last_seen: Utc::now(),
        }];

        let analysis = ConvergenceAnalysis::analyze(&existing, &report);
        assert_eq!(analysis.strategy, RetryStrategy::ExpandedContext);
        assert_eq!(analysis.repeated_signatures.len(), 1);
    }

    #[test]
    fn convergence_analysis_escalates_to_different_approach() {
        let report = make_gate_report(vec![("cargo_clippy", false, "error: clone")]);
        let sigs = FailureSignature::from_gate_report(&report);

        let existing = vec![FailureRecord {
            task_id: TaskId(1),
            signature: sigs[0].clone(),
            occurrence_count: 2,
            latest_stderr: "error: clone".into(),
            first_seen: Utc::now(),
            last_seen: Utc::now(),
        }];

        let analysis = ConvergenceAnalysis::analyze(&existing, &report);
        assert_eq!(analysis.strategy, RetryStrategy::DifferentApproach);
    }

    #[test]
    fn convergence_analysis_flags_human_review() {
        let report = make_gate_report(vec![("cargo_test", false, "timeout")]);
        let sigs = FailureSignature::from_gate_report(&report);

        let existing = vec![FailureRecord {
            task_id: TaskId(1),
            signature: sigs[0].clone(),
            occurrence_count: 3,
            latest_stderr: "timeout".into(),
            first_seen: Utc::now(),
            last_seen: Utc::now(),
        }];

        let analysis = ConvergenceAnalysis::analyze(&existing, &report);
        assert_eq!(analysis.strategy, RetryStrategy::HumanReview);
        assert!(analysis.strategy.needs_human());
    }

    #[test]
    fn failure_record_occurrence_tracking() {
        let sig = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            error_hash: "abc123".into(),
        };
        let mut record = FailureRecord::new(TaskId(1), sig, "first error".into());
        assert_eq!(record.occurrence_count, 1);

        record.record_occurrence("second error".into());
        assert_eq!(record.occurrence_count, 2);
        assert_eq!(record.latest_stderr, "second error");
    }

    #[test]
    fn normalize_error_strips_line_numbers() {
        let err1 = "error[E0599]: src/lib.rs:42:5 no method named `foo`";
        let err2 = "error[E0599]: src/lib.rs:99:10 no method named `foo`";
        // Should produce the same hash since the error is the same
        assert_eq!(normalize_error(err1), normalize_error(err2));
    }

    #[test]
    fn normalize_error_same_error_same_hash() {
        let err = "error: unused variable `x`";
        let h1 = normalize_error(err);
        let h2 = normalize_error(err);
        assert_eq!(h1, h2);
    }

    #[test]
    fn normalize_error_different_errors_different_hash() {
        let err1 = "error: unused variable `x`";
        let err2 = "error: missing lifetime specifier";
        assert_ne!(normalize_error(err1), normalize_error(err2));
    }

    #[test]
    fn strip_ansi_removes_escape_codes() {
        let with_ansi = "\x1b[31merror\x1b[0m: something failed";
        let clean = strip_ansi(with_ansi);
        assert_eq!(clean, "error: something failed");
    }

    #[test]
    fn strategy_labels() {
        assert_eq!(RetryStrategy::Normal.label(), "normal");
        assert_eq!(RetryStrategy::ExpandedContext.label(), "expanded-context");
        assert_eq!(
            RetryStrategy::DifferentApproach.label(),
            "different-approach"
        );
        assert_eq!(RetryStrategy::HumanReview.label(), "human-review");
    }

    #[test]
    fn prompt_augmentation_normal_is_empty() {
        let aug = RetryStrategy::Normal.prompt_augmentation(&[]);
        assert!(aug.is_empty());
    }

    #[test]
    fn prompt_augmentation_expanded_context() {
        let sig = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            error_hash: "abc".into(),
        };
        let aug = RetryStrategy::ExpandedContext.prompt_augmentation(&[sig]);
        assert!(aug.contains("Convergence Warning"));
        assert!(aug.contains("Expanded Context"));
        assert!(aug.contains("cargo_test"));
    }

    #[test]
    fn prompt_augmentation_different_approach() {
        let sig = FailureSignature {
            gate_level: GateLevel::Proof,
            check_name: "z3_verify".into(),
            error_hash: "def".into(),
        };
        let aug = RetryStrategy::DifferentApproach.prompt_augmentation(&[sig]);
        assert!(aug.contains("Strategy Rotation"));
        assert!(aug.contains("NOT working"));
        assert!(aug.contains("z3_verify"));
    }
}
