//! CI status types shared between core and runner.

use serde::{Deserialize, Serialize};

/// Status of a single CI check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CICheck {
    /// Name of the check (e.g. "build", "test", "lint").
    pub name: String,
    /// Status: "pending", "pass", "fail", "cancelled", "skipped".
    pub status: String,
    /// Optional URL to the check run details.
    pub url: Option<String>,
}

/// Aggregated CI status for a PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CIStatus {
    /// Some checks are still running.
    Pending,
    /// All checks passed.
    Pass,
    /// At least one check failed.
    Fail,
    /// No checks found (CI may not be configured).
    NoChecks,
}

impl std::fmt::Display for CIStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CIStatus::Pending => write!(f, "pending"),
            CIStatus::Pass => write!(f, "pass"),
            CIStatus::Fail => write!(f, "fail"),
            CIStatus::NoChecks => write!(f, "no-checks"),
        }
    }
}

/// Result of polling CI status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CIPollResult {
    pub status: CIStatus,
    pub checks: Vec<CICheck>,
    /// Human-readable summary.
    pub summary: String,
}
