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

impl CIPollResult {
    /// Build a poll result from a list of checks.
    ///
    /// Automatically aggregates individual check statuses into an overall status:
    /// - Any pending/queued/in_progress → `Pending`
    /// - Any failure/error (and none pending) → `Fail`
    /// - All success/skipped → `Pass`
    /// - Empty checks → `NoChecks`
    pub fn from_checks(checks: Vec<CICheck>) -> Self {
        if checks.is_empty() {
            return Self {
                status: CIStatus::NoChecks,
                checks,
                summary: "No CI checks found".into(),
            };
        }

        let any_pending = checks.iter().any(|c| {
            matches!(
                c.status.as_str(),
                "pending" | "queued" | "in_progress" | "waiting"
            )
        });
        let any_failed = checks
            .iter()
            .any(|c| matches!(c.status.as_str(), "failure" | "error" | "cancelled"));

        let status = if any_pending {
            CIStatus::Pending
        } else if any_failed {
            CIStatus::Fail
        } else {
            CIStatus::Pass
        };

        let passed = checks.iter().filter(|c| c.status == "success").count();
        let failed = checks
            .iter()
            .filter(|c| c.status == "failure" || c.status == "error")
            .count();
        let pending = checks.len() - passed - failed;

        let summary = format!(
            "{passed} passed, {failed} failed, {pending} pending (total: {})",
            checks.len()
        );

        Self {
            status,
            checks,
            summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_status_display_variants() {
        assert_eq!(CIStatus::Pending.to_string(), "pending");
        assert_eq!(CIStatus::Pass.to_string(), "pass");
        assert_eq!(CIStatus::Fail.to_string(), "fail");
        assert_eq!(CIStatus::NoChecks.to_string(), "no-checks");
    }

    #[test]
    fn ci_status_equality() {
        assert_eq!(CIStatus::Pass, CIStatus::Pass);
        assert_ne!(CIStatus::Pass, CIStatus::Fail);
        assert_ne!(CIStatus::Pending, CIStatus::NoChecks);
    }

    #[test]
    fn ci_check_serialize_roundtrip() {
        let check = CICheck {
            name: "build".into(),
            status: "success".into(),
            url: Some("https://github.com/org/repo/actions/runs/123".into()),
        };
        let json = serde_json::to_string(&check).unwrap();
        let parsed: CICheck = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "build");
        assert_eq!(parsed.status, "success");
        assert!(parsed.url.is_some());
    }

    #[test]
    fn ci_check_without_url() {
        let check = CICheck {
            name: "lint".into(),
            status: "pending".into(),
            url: None,
        };
        let json = serde_json::to_string(&check).unwrap();
        let parsed: CICheck = serde_json::from_str(&json).unwrap();
        assert!(parsed.url.is_none());
    }

    #[test]
    fn ci_poll_result_from_empty_checks() {
        let result = CIPollResult::from_checks(vec![]);
        assert_eq!(result.status, CIStatus::NoChecks);
        assert!(result.checks.is_empty());
    }

    #[test]
    fn ci_poll_result_all_passing() {
        let checks = vec![
            CICheck {
                name: "build".into(),
                status: "success".into(),
                url: None,
            },
            CICheck {
                name: "test".into(),
                status: "success".into(),
                url: None,
            },
            CICheck {
                name: "lint".into(),
                status: "success".into(),
                url: None,
            },
        ];
        let result = CIPollResult::from_checks(checks);
        assert_eq!(result.status, CIStatus::Pass);
        assert!(result.summary.contains("3 passed"));
        assert!(result.summary.contains("0 failed"));
    }

    #[test]
    fn ci_poll_result_with_failure() {
        let checks = vec![
            CICheck {
                name: "build".into(),
                status: "success".into(),
                url: None,
            },
            CICheck {
                name: "test".into(),
                status: "failure".into(),
                url: Some("https://example.com/run/456".into()),
            },
        ];
        let result = CIPollResult::from_checks(checks);
        assert_eq!(result.status, CIStatus::Fail);
        assert!(result.summary.contains("1 failed"));
    }

    #[test]
    fn ci_poll_result_pending_takes_priority() {
        let checks = vec![
            CICheck {
                name: "build".into(),
                status: "failure".into(),
                url: None,
            },
            CICheck {
                name: "test".into(),
                status: "pending".into(),
                url: None,
            },
        ];
        let result = CIPollResult::from_checks(checks);
        assert_eq!(result.status, CIStatus::Pending);
    }

    #[test]
    fn ci_poll_result_queued_counts_as_pending() {
        let checks = vec![CICheck {
            name: "deploy".into(),
            status: "queued".into(),
            url: None,
        }];
        let result = CIPollResult::from_checks(checks);
        assert_eq!(result.status, CIStatus::Pending);
    }

    #[test]
    fn ci_poll_result_error_counts_as_failure() {
        let checks = vec![CICheck {
            name: "build".into(),
            status: "error".into(),
            url: None,
        }];
        let result = CIPollResult::from_checks(checks);
        assert_eq!(result.status, CIStatus::Fail);
    }

    #[test]
    fn ci_poll_result_cancelled_counts_as_failure() {
        let checks = vec![
            CICheck {
                name: "build".into(),
                status: "success".into(),
                url: None,
            },
            CICheck {
                name: "deploy".into(),
                status: "cancelled".into(),
                url: None,
            },
        ];
        let result = CIPollResult::from_checks(checks);
        assert_eq!(result.status, CIStatus::Fail);
    }

    #[test]
    fn ci_poll_result_serialize_roundtrip() {
        let result = CIPollResult {
            status: CIStatus::Pass,
            checks: vec![CICheck {
                name: "test".into(),
                status: "success".into(),
                url: None,
            }],
            summary: "1 passed, 0 failed, 0 pending (total: 1)".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: CIPollResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, CIStatus::Pass);
        assert_eq!(parsed.checks.len(), 1);
        assert_eq!(parsed.summary, result.summary);
    }

    #[test]
    fn ci_poll_result_skipped_checks_count_as_pass() {
        let checks = vec![
            CICheck {
                name: "build".into(),
                status: "success".into(),
                url: None,
            },
            CICheck {
                name: "optional-lint".into(),
                status: "skipped".into(),
                url: None,
            },
        ];
        let result = CIPollResult::from_checks(checks);
        assert_eq!(result.status, CIStatus::Pass);
    }
}
