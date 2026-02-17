//! Verification-tagged acceptance criteria for harness-first engineering.
//!
//! Each acceptance criterion gets a verification tag specifying HOW it will be
//! verified: (TEST), (LINT), (BENCH), (MANUAL), (BROWSER), (SECURITY).
//!
//! This creates traceability from requirement → verification method → result.
//! "Hope someone reads the code" is not acceptable.

use serde::{Deserialize, Serialize};

/// How an acceptance criterion will be verified.
///
/// Inspired by harness-first engineering (Shoemaker): if it matters,
/// there must be a concrete, automated verification mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VerificationTag {
    /// Verified by automated tests (unit, integration, property-based).
    Test,
    /// Verified by linting / static analysis (clippy, eslint, etc.).
    Lint,
    /// Verified by benchmarks / performance tests.
    Bench,
    /// Requires manual human verification.
    Manual,
    /// Verified by browser / UI testing.
    Browser,
    /// Verified by security audit / scanning.
    Security,
}

impl VerificationTag {
    /// Parse a tag from its string representation (case-insensitive).
    pub fn from_str_tag(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "TEST" => Some(Self::Test),
            "LINT" => Some(Self::Lint),
            "BENCH" => Some(Self::Bench),
            "MANUAL" => Some(Self::Manual),
            "BROWSER" => Some(Self::Browser),
            "SECURITY" => Some(Self::Security),
            _ => None,
        }
    }

    /// The canonical string form used in criteria text, e.g. "(TEST)".
    pub fn as_tag_str(&self) -> &'static str {
        match self {
            Self::Test => "(TEST)",
            Self::Lint => "(LINT)",
            Self::Bench => "(BENCH)",
            Self::Manual => "(MANUAL)",
            Self::Browser => "(BROWSER)",
            Self::Security => "(SECURITY)",
        }
    }

    /// All valid verification tags.
    pub fn all() -> &'static [VerificationTag] {
        &[
            Self::Test,
            Self::Lint,
            Self::Bench,
            Self::Manual,
            Self::Browser,
            Self::Security,
        ]
    }

    /// Gate check names that correspond to this verification tag.
    ///
    /// Used to map gate results back to tagged criteria.
    pub fn matching_check_names(&self) -> &'static [&'static str] {
        match self {
            Self::Test => &["cargo_test", "test", "integration_test"],
            Self::Lint => &["cargo_clippy", "cargo_fmt", "clippy", "fmt", "lint"],
            Self::Bench => &["bench", "benchmark", "perf"],
            Self::Manual => &["manual", "review"],
            Self::Browser => &["browser", "e2e", "playwright", "cypress"],
            Self::Security => &["security", "audit", "cargo_audit", "advisory"],
        }
    }
}

impl std::fmt::Display for VerificationTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_tag_str())
    }
}

/// An acceptance criterion with a verification tag and tracked results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaggedCriterion {
    /// The human-readable criterion text (without the tag suffix).
    pub description: String,
    /// How this criterion will be verified.
    pub tag: VerificationTag,
    /// Verification results (populated as gates run).
    #[serde(default)]
    pub verifications: Vec<CriterionVerification>,
}

impl TaggedCriterion {
    /// Format the criterion as a tagged string, e.g. "Tests pass (TEST)".
    pub fn to_tagged_string(&self) -> String {
        format!("{} {}", self.description, self.tag.as_tag_str())
    }

    /// Whether this criterion has been verified (at least one passing verification).
    pub fn is_verified(&self) -> bool {
        self.verifications.iter().any(|v| v.passed)
    }

    /// Whether this criterion was checked but failed.
    pub fn is_failed(&self) -> bool {
        !self.verifications.is_empty() && !self.is_verified()
    }

    /// Status label for display.
    pub fn status_label(&self) -> &'static str {
        if self.is_verified() {
            "verified"
        } else if self.is_failed() {
            "failed"
        } else {
            "pending"
        }
    }
}

/// A single verification result for a criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionVerification {
    /// Which gate check produced this result (e.g. "cargo_test").
    pub check_name: String,
    /// Whether the verification passed.
    pub passed: bool,
    /// When the verification ran.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

// ─── Parsing ────────────────────────────────────────────────────────────

/// Parse a tagged criterion from a string like "Tests pass (TEST)".
///
/// Returns `None` if no valid tag is found at the end.
pub fn parse_tagged_criterion(s: &str) -> Option<TaggedCriterion> {
    let trimmed = s.trim();

    // Look for a parenthesized tag at the end, e.g. "(TEST)"
    if let Some(open) = trimmed.rfind('(')
        && trimmed.ends_with(')')
    {
        let tag_str = &trimmed[open + 1..trimmed.len() - 1];
        if let Some(tag) = VerificationTag::from_str_tag(tag_str) {
            let description = trimmed[..open].trim().to_string();
            return Some(TaggedCriterion {
                description,
                tag,
                verifications: Vec::new(),
            });
        }
    }

    None
}

/// Parse all criteria from string list, returning tagged ones and errors.
pub fn parse_all_criteria(criteria: &[String]) -> (Vec<TaggedCriterion>, Vec<String>) {
    let mut tagged = Vec::new();
    let mut untagged = Vec::new();

    for criterion in criteria {
        match parse_tagged_criterion(criterion) {
            Some(tc) => tagged.push(tc),
            None => untagged.push(criterion.clone()),
        }
    }

    (tagged, untagged)
}

// ─── Pre-dispatch audit ─────────────────────────────────────────────────

/// Result of auditing a task's acceptance criteria before dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditResult {
    /// Whether the audit passed (all criteria are tagged and concrete).
    pub passed: bool,
    /// Feedback messages for the user/planner.
    pub feedback: Vec<String>,
    /// Successfully parsed tagged criteria.
    pub tagged_criteria: Vec<TaggedCriterion>,
}

/// Audit acceptance criteria before a task moves from Pending to Implementing.
///
/// Validates that:
/// 1. Every criterion has a verification tag.
/// 2. No criterion is vague (e.g. "make it better").
///
/// Returns an `AuditResult` with feedback if the audit fails.
pub fn audit_criteria(criteria: &[String]) -> AuditResult {
    if criteria.is_empty() {
        return AuditResult {
            passed: true,
            feedback: vec![
                "No acceptance criteria defined — task will proceed without criteria.".into(),
            ],
            tagged_criteria: Vec::new(),
        };
    }

    let (tagged, untagged) = parse_all_criteria(criteria);
    let mut feedback = Vec::new();

    // Check for untagged criteria
    for criterion in &untagged {
        feedback.push(format!(
            "Untagged criterion: \"{criterion}\". Add a verification tag like (TEST), (LINT), (BENCH), (MANUAL), (BROWSER), or (SECURITY)."
        ));
    }

    // Check for vague criteria
    let vague_patterns = [
        "make it better",
        "improve",
        "fix stuff",
        "clean up",
        "looks good",
        "should work",
    ];

    for tc in &tagged {
        let lower = tc.description.to_lowercase();
        for pattern in &vague_patterns {
            if lower.contains(pattern) {
                feedback.push(format!(
                    "Vague criterion: \"{}\". Make it concrete and measurable.",
                    tc.description
                ));
                break;
            }
        }
    }

    let passed = untagged.is_empty() && feedback.is_empty();

    AuditResult {
        passed,
        feedback,
        tagged_criteria: tagged,
    }
}

// ─── Gate result mapping ────────────────────────────────────────────────

/// Map gate check results to tagged criteria, recording which criteria
/// were verified (or failed) by which checks.
///
/// Returns the updated criteria with verification results attached.
pub fn map_gate_results(
    criteria: &[TaggedCriterion],
    checks: &[crate::task::CheckResult],
) -> Vec<TaggedCriterion> {
    let now = chrono::Utc::now();

    criteria
        .iter()
        .map(|tc| {
            let mut updated = tc.clone();
            let matching_names = tc.tag.matching_check_names();

            for check in checks {
                let check_lower = check.name.to_lowercase();
                let matches = matching_names.iter().any(|name| check_lower.contains(name));

                if matches {
                    updated.verifications.push(CriterionVerification {
                        check_name: check.name.clone(),
                        passed: check.passed,
                        timestamp: now,
                    });
                }
            }

            updated
        })
        .collect()
}

/// Generate a verification summary for display.
///
/// Returns (verified_count, failed_count, pending_count, total).
pub fn verification_summary(criteria: &[TaggedCriterion]) -> (usize, usize, usize, usize) {
    let total = criteria.len();
    let verified = criteria.iter().filter(|c| c.is_verified()).count();
    let failed = criteria.iter().filter(|c| c.is_failed()).count();
    let pending = total - verified - failed;
    (verified, failed, pending, total)
}

// ─── Planner enrichment ─────────────────────────────────────────────────

/// Suggest verification tags for untagged criteria based on keywords.
///
/// This is a best-effort heuristic — the planner agent should do the real
/// enrichment using LLM intelligence.
pub fn suggest_tag(criterion: &str) -> VerificationTag {
    let lower = criterion.to_lowercase();

    if lower.contains("clippy")
        || lower.contains("lint")
        || lower.contains("fmt")
        || lower.contains("format")
        || lower.contains("warning")
    {
        VerificationTag::Lint
    } else if lower.contains("bench")
        || lower.contains("latency")
        || lower.contains("throughput")
        || lower.contains("p99")
        || lower.contains("p95")
        || lower.contains("perf")
    {
        VerificationTag::Bench
    } else if lower.contains("browser")
        || lower.contains("ui")
        || lower.contains("render")
        || lower.contains("display")
        || lower.contains("dashboard")
        || lower.contains("visible")
    {
        VerificationTag::Browser
    } else if lower.contains("security")
        || lower.contains("auth")
        || lower.contains("cve")
        || lower.contains("vulnerability")
        || lower.contains("xss")
        || lower.contains("injection")
    {
        VerificationTag::Security
    } else if lower.contains("manual")
        || lower.contains("review")
        || lower.contains("inspect")
        || lower.contains("human")
    {
        VerificationTag::Manual
    } else {
        // Default: most criteria are verifiable by tests
        VerificationTag::Test
    }
}

/// Enrich untagged criteria by adding suggested verification tags.
///
/// Already-tagged criteria are preserved as-is.
pub fn enrich_criteria(criteria: &[String]) -> Vec<String> {
    criteria
        .iter()
        .map(|c| {
            if parse_tagged_criterion(c).is_some() {
                // Already tagged
                c.clone()
            } else {
                // Add suggested tag
                let tag = suggest_tag(c);
                format!("{} {}", c.trim(), tag.as_tag_str())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_test_tag() {
        let tc = parse_tagged_criterion("All tests pass (TEST)").unwrap();
        assert_eq!(tc.description, "All tests pass");
        assert_eq!(tc.tag, VerificationTag::Test);
        assert!(tc.verifications.is_empty());
    }

    #[test]
    fn parse_lint_tag() {
        let tc = parse_tagged_criterion("No clippy warnings (LINT)").unwrap();
        assert_eq!(tc.description, "No clippy warnings");
        assert_eq!(tc.tag, VerificationTag::Lint);
    }

    #[test]
    fn parse_bench_tag() {
        let tc = parse_tagged_criterion("P99 latency below 50ms on /api/tasks (BENCH)").unwrap();
        assert_eq!(tc.description, "P99 latency below 50ms on /api/tasks");
        assert_eq!(tc.tag, VerificationTag::Bench);
    }

    #[test]
    fn parse_all_tags() {
        for tag in VerificationTag::all() {
            let input = format!("Some criterion {}", tag.as_tag_str());
            let tc = parse_tagged_criterion(&input).unwrap();
            assert_eq!(tc.tag, *tag);
        }
    }

    #[test]
    fn parse_no_tag_returns_none() {
        assert!(parse_tagged_criterion("Just some text").is_none());
        assert!(parse_tagged_criterion("Has parens (but invalid)").is_none());
    }

    #[test]
    fn parse_case_insensitive() {
        let tc = parse_tagged_criterion("Tests pass (test)").unwrap();
        assert_eq!(tc.tag, VerificationTag::Test);

        let tc = parse_tagged_criterion("Lint clean (Lint)").unwrap();
        assert_eq!(tc.tag, VerificationTag::Lint);
    }

    #[test]
    fn parse_all_criteria_mixed() {
        let criteria = vec![
            "Tests pass (TEST)".into(),
            "Untagged criterion".into(),
            "No warnings (LINT)".into(),
        ];
        let (tagged, untagged) = parse_all_criteria(&criteria);
        assert_eq!(tagged.len(), 2);
        assert_eq!(untagged.len(), 1);
        assert_eq!(untagged[0], "Untagged criterion");
    }

    #[test]
    fn audit_all_tagged_passes() {
        let criteria = vec!["Tests pass (TEST)".into(), "No warnings (LINT)".into()];
        let result = audit_criteria(&criteria);
        assert!(result.passed);
        assert_eq!(result.tagged_criteria.len(), 2);
    }

    #[test]
    fn audit_untagged_fails() {
        let criteria = vec!["Tests pass (TEST)".into(), "Some untagged thing".into()];
        let result = audit_criteria(&criteria);
        assert!(!result.passed);
        assert!(!result.feedback.is_empty());
    }

    #[test]
    fn audit_vague_fails() {
        let criteria = vec!["Make it better (TEST)".into()];
        let result = audit_criteria(&criteria);
        assert!(!result.passed);
        assert!(result.feedback[0].contains("Vague"));
    }

    #[test]
    fn audit_empty_passes() {
        let result = audit_criteria(&[]);
        assert!(result.passed);
    }

    #[test]
    fn suggest_tag_keywords() {
        assert_eq!(suggest_tag("No clippy warnings"), VerificationTag::Lint);
        assert_eq!(
            suggest_tag("P99 latency below 50ms"),
            VerificationTag::Bench
        );
        assert_eq!(
            suggest_tag("Dashboard shows status"),
            VerificationTag::Browser
        );
        assert_eq!(
            suggest_tag("No XSS vulnerabilities"),
            VerificationTag::Security
        );
        assert_eq!(
            suggest_tag("Manual review of docs"),
            VerificationTag::Manual
        );
        assert_eq!(suggest_tag("All unit tests pass"), VerificationTag::Test);
    }

    #[test]
    fn enrich_adds_tags() {
        let criteria = vec![
            "Tests pass (TEST)".into(),
            "No clippy warnings".into(),
            "P99 latency below 50ms".into(),
        ];
        let enriched = enrich_criteria(&criteria);
        assert_eq!(enriched[0], "Tests pass (TEST)");
        assert!(enriched[1].ends_with("(LINT)"));
        assert!(enriched[2].ends_with("(BENCH)"));
    }

    #[test]
    fn map_gate_results_links_checks() {
        let criteria = vec![
            TaggedCriterion {
                description: "Tests pass".into(),
                tag: VerificationTag::Test,
                verifications: Vec::new(),
            },
            TaggedCriterion {
                description: "No warnings".into(),
                tag: VerificationTag::Lint,
                verifications: Vec::new(),
            },
        ];

        let checks = vec![
            crate::task::CheckResult {
                name: "cargo_test".into(),
                passed: true,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            },
            crate::task::CheckResult {
                name: "cargo_clippy".into(),
                passed: false,
                stdout: String::new(),
                stderr: "warning found".into(),
                exit_code: 1,
            },
        ];

        let updated = map_gate_results(&criteria, &checks);
        assert_eq!(updated[0].verifications.len(), 1);
        assert!(updated[0].verifications[0].passed);
        assert_eq!(updated[0].verifications[0].check_name, "cargo_test");

        assert_eq!(updated[1].verifications.len(), 1);
        assert!(!updated[1].verifications[0].passed);
        assert_eq!(updated[1].verifications[0].check_name, "cargo_clippy");
    }

    #[test]
    fn verification_summary_counts() {
        let criteria = vec![
            TaggedCriterion {
                description: "Tests pass".into(),
                tag: VerificationTag::Test,
                verifications: vec![CriterionVerification {
                    check_name: "cargo_test".into(),
                    passed: true,
                    timestamp: chrono::Utc::now(),
                }],
            },
            TaggedCriterion {
                description: "No warnings".into(),
                tag: VerificationTag::Lint,
                verifications: vec![CriterionVerification {
                    check_name: "cargo_clippy".into(),
                    passed: false,
                    timestamp: chrono::Utc::now(),
                }],
            },
            TaggedCriterion {
                description: "Perf ok".into(),
                tag: VerificationTag::Bench,
                verifications: Vec::new(),
            },
        ];

        let (verified, failed, pending, total) = verification_summary(&criteria);
        assert_eq!(verified, 1);
        assert_eq!(failed, 1);
        assert_eq!(pending, 1);
        assert_eq!(total, 3);
    }

    #[test]
    fn tagged_criterion_status_labels() {
        let mut tc = TaggedCriterion {
            description: "Test".into(),
            tag: VerificationTag::Test,
            verifications: Vec::new(),
        };
        assert_eq!(tc.status_label(), "pending");

        tc.verifications.push(CriterionVerification {
            check_name: "test".into(),
            passed: false,
            timestamp: chrono::Utc::now(),
        });
        assert_eq!(tc.status_label(), "failed");

        tc.verifications.push(CriterionVerification {
            check_name: "test".into(),
            passed: true,
            timestamp: chrono::Utc::now(),
        });
        assert_eq!(tc.status_label(), "verified");
    }

    #[test]
    fn verification_tag_display() {
        assert_eq!(format!("{}", VerificationTag::Test), "(TEST)");
        assert_eq!(format!("{}", VerificationTag::Lint), "(LINT)");
        assert_eq!(format!("{}", VerificationTag::Bench), "(BENCH)");
        assert_eq!(format!("{}", VerificationTag::Manual), "(MANUAL)");
        assert_eq!(format!("{}", VerificationTag::Browser), "(BROWSER)");
        assert_eq!(format!("{}", VerificationTag::Security), "(SECURITY)");
    }

    #[test]
    fn tagged_criterion_to_string() {
        let tc = TaggedCriterion {
            description: "All tests pass".into(),
            tag: VerificationTag::Test,
            verifications: Vec::new(),
        };
        assert_eq!(tc.to_tagged_string(), "All tests pass (TEST)");
    }
}
