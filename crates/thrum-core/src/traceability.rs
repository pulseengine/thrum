use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A record in the functional safety traceability chain:
/// Requirement -> Design -> Implementation -> Test -> Proof -> Release
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    pub id: i64,
    pub task_id: i64,
    pub requirement_id: String,
    pub artifact: TraceArtifact,
    pub created_at: DateTime<Utc>,
}

/// Types of traceable artifacts in the verification chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceArtifact {
    Requirement {
        title: String,
        description: String,
    },
    Design {
        rationale: String,
    },
    Implementation {
        branch: String,
        commit_sha: Option<String>,
        files_changed: Vec<String>,
    },
    Test {
        gate_level: String,
        passed: bool,
        report_json: String,
    },
    Proof {
        prover: String, // "z3", "rocq"
        passed: bool,
        report_json: String,
    },
    Review {
        reviewer: String,
        approved: bool,
        comments: String,
    },
    Release {
        version: String,
        targets: Vec<String>,
        checksums: Vec<(String, String)>,
    },
}

// TCL is defined in crate::safety::Tcl — use that for tool classification.

/// Full traceability matrix for a release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceabilityMatrix {
    pub tool: String,
    pub version: String,
    pub entries: Vec<TraceMatrixEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMatrixEntry {
    pub requirement_id: String,
    pub design: Option<String>,
    pub implementation_commit: Option<String>,
    pub test_status: Option<bool>,
    pub proof_status: Option<bool>,
    pub review_status: Option<bool>,
}

impl TraceabilityMatrix {
    /// Export as CSV (for certification documentation).
    pub fn to_csv(&self) -> String {
        let mut out = String::from("requirement_id,design,implementation,test,proof,review\n");
        for entry in &self.entries {
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                entry.requirement_id,
                entry.design.as_deref().unwrap_or("—"),
                entry.implementation_commit.as_deref().unwrap_or("—"),
                entry.test_status.map_or("—".into(), |b| b.to_string()),
                entry.proof_status.map_or("—".into(), |b| b.to_string()),
                entry.review_status.map_or("—".into(), |b| b.to_string()),
            ));
        }
        out
    }
}
