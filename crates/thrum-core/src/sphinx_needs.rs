//! Export traceability data in sphinx-needs format.
//!
//! Generates `needs.json` files compatible with sphinx-needs `needimport` directive.
//! This enables full requirements traceability documentation built by Sphinx.
//!
//! V-model chain: REQ → ARCH → IMPL → UTEST → PROOF → ITEST → REVIEW → VERIF → REL
//!
//! Each maps to an ASPICE SWE process:
//!   req → SWE.1, arch → SWE.2, impl → SWE.3, utest → SWE.4,
//!   itest → SWE.5, qtest → SWE.6

use crate::traceability::{TraceArtifact, TraceRecord};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Need Types (matching sphinx-needs configuration) ──────────────────

/// All need types in the traceability chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NeedType {
    Req,
    Arch,
    Design,
    Impl,
    Utest,
    Itest,
    Proof,
    Review,
    Verif,
    Rel,
}

impl NeedType {
    pub fn label(&self) -> &'static str {
        match self {
            NeedType::Req => "Requirement",
            NeedType::Arch => "Architecture",
            NeedType::Design => "Detailed Design",
            NeedType::Impl => "Implementation",
            NeedType::Utest => "Unit Test",
            NeedType::Itest => "Integration Test",
            NeedType::Proof => "Formal Proof",
            NeedType::Review => "Code Review",
            NeedType::Verif => "Verification Report",
            NeedType::Rel => "Release Artifact",
        }
    }

    pub fn directive(&self) -> &'static str {
        match self {
            NeedType::Req => "req",
            NeedType::Arch => "arch",
            NeedType::Design => "design",
            NeedType::Impl => "impl",
            NeedType::Utest => "utest",
            NeedType::Itest => "itest",
            NeedType::Proof => "proof",
            NeedType::Review => "review",
            NeedType::Verif => "verif",
            NeedType::Rel => "rel",
        }
    }

    /// ASPICE process this need type corresponds to.
    pub fn aspice_process(&self) -> &'static str {
        match self {
            NeedType::Req => "SWE.1",
            NeedType::Arch => "SWE.2",
            NeedType::Design => "SWE.3",
            NeedType::Impl => "SWE.3",
            NeedType::Utest => "SWE.4",
            NeedType::Itest => "SWE.5",
            NeedType::Proof => "SWE.4",
            NeedType::Review => "SWE.4",
            NeedType::Verif => "SWE.6",
            NeedType::Rel => "SWE.6",
        }
    }
}

// ─── Need (sphinx-needs compatible) ────────────────────────────────────

/// A single need item compatible with sphinx-needs JSON format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Need {
    /// Unique identifier, e.g. "REQ_LOOM_042", "IMPL_LOOM_042_1".
    pub id: String,
    /// Need type (req, arch, impl, etc.).
    #[serde(rename = "type")]
    pub need_type: String,
    /// Human-readable title.
    pub title: String,
    /// Description / content.
    pub description: String,
    /// Current status.
    pub status: String,
    /// Links to other needs (traceability).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// Tags for filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Custom fields.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, String>,
}

// ─── needs.json export format ──────────────────────────────────────────

/// Top-level structure of a needs.json file (sphinx-needs needimport format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeedsJson {
    /// Project name.
    pub project: String,
    /// Export version.
    pub version: String,
    /// All needs keyed by their ID.
    pub needs: HashMap<String, Need>,
    /// Metadata about the export.
    #[serde(default)]
    pub created: String,
}

impl NeedsJson {
    pub fn new(project: &str, version: &str) -> Self {
        Self {
            project: project.to_string(),
            version: version.to_string(),
            needs: HashMap::new(),
            created: Utc::now().to_rfc3339(),
        }
    }

    pub fn add(&mut self, need: Need) {
        self.needs.insert(need.id.clone(), need);
    }

    /// Export to JSON string.
    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

// ─── Conversion from TraceRecord to Need ───────────────────────────────

/// Convert a TraceRecord into one or more sphinx-needs Need items.
pub fn trace_record_to_needs(record: &TraceRecord) -> Vec<Need> {
    let base_id = sanitize_id(&record.requirement_id);
    let task_suffix = format!("T{}", record.task_id);

    match &record.artifact {
        TraceArtifact::Requirement { title, description } => {
            vec![Need {
                id: base_id,
                need_type: NeedType::Req.directive().to_string(),
                title: title.clone(),
                description: description.clone(),
                status: "open".into(),
                links: vec![],
                tags: vec!["requirement".into()],
                extra: HashMap::new(),
            }]
        }
        TraceArtifact::Design { rationale } => {
            let id = format!("{base_id}_ARCH_{task_suffix}");
            vec![Need {
                id,
                need_type: NeedType::Arch.directive().to_string(),
                title: format!("Architecture for {}", record.requirement_id),
                description: rationale.clone(),
                status: "open".into(),
                links: vec![base_id],
                tags: vec!["architecture".into()],
                extra: HashMap::new(),
            }]
        }
        TraceArtifact::Implementation {
            branch,
            commit_sha,
            files_changed,
        } => {
            let id = format!("{base_id}_IMPL_{task_suffix}");
            let mut extra = HashMap::new();
            extra.insert("branch".into(), branch.clone());
            if let Some(sha) = commit_sha {
                extra.insert("commit".into(), sha.clone());
            }
            extra.insert("files".into(), files_changed.join(", "));

            vec![Need {
                id,
                need_type: NeedType::Impl.directive().to_string(),
                title: format!("Implementation for {}", record.requirement_id),
                description: format!("Branch: {}\nFiles: {}", branch, files_changed.join(", ")),
                status: "implemented".into(),
                links: vec![base_id],
                tags: vec!["implementation".into()],
                extra,
            }]
        }
        TraceArtifact::Test {
            gate_level,
            passed,
            report_json,
        } => {
            let need_type = if gate_level.contains("Integration") || gate_level.contains("3") {
                NeedType::Itest
            } else {
                NeedType::Utest
            };
            let id = format!(
                "{}_{}_{}",
                base_id,
                need_type.directive().to_uppercase(),
                task_suffix
            );
            let status = if *passed { "passed" } else { "failed" };

            let mut extra = HashMap::new();
            extra.insert("gate_level".into(), gate_level.clone());
            // Store a truncated version of the report
            if report_json.len() <= 1000 {
                extra.insert("report".into(), report_json.clone());
            }

            vec![Need {
                id,
                need_type: need_type.directive().to_string(),
                title: format!("{} for {}", need_type.label(), record.requirement_id),
                description: format!("Gate: {} — {}", gate_level, status),
                status: status.into(),
                links: vec![base_id],
                tags: vec!["test".into(), gate_level.clone()],
                extra,
            }]
        }
        TraceArtifact::Proof {
            prover,
            passed,
            report_json,
        } => {
            let id = format!("{base_id}_PROOF_{task_suffix}");
            let status = if *passed { "verified" } else { "failed" };
            let mut extra = HashMap::new();
            extra.insert("prover".into(), prover.clone());
            if report_json.len() <= 1000 {
                extra.insert("report".into(), report_json.clone());
            }

            vec![Need {
                id,
                need_type: NeedType::Proof.directive().to_string(),
                title: format!("Formal proof ({}) for {}", prover, record.requirement_id),
                description: format!("Prover: {} — {}", prover, status),
                status: status.into(),
                links: vec![base_id],
                tags: vec!["proof".into(), prover.clone()],
                extra,
            }]
        }
        TraceArtifact::Review {
            reviewer,
            approved,
            comments,
        } => {
            let id = format!("{base_id}_REVIEW_{task_suffix}");
            let status = if *approved {
                "approved"
            } else {
                "changes_requested"
            };

            vec![Need {
                id,
                need_type: NeedType::Review.directive().to_string(),
                title: format!("Code review for {}", record.requirement_id),
                description: comments.clone(),
                status: status.into(),
                links: vec![base_id],
                tags: vec!["review".into(), reviewer.clone()],
                extra: HashMap::from([("reviewer".into(), reviewer.clone())]),
            }]
        }
        TraceArtifact::Release {
            version,
            targets,
            checksums,
        } => {
            let id = format!("{base_id}_REL_{}", sanitize_id(version));
            let mut extra = HashMap::new();
            extra.insert("targets".into(), targets.join(", "));
            for (file, hash) in checksums {
                extra.insert(format!("sha256_{file}"), hash.clone());
            }

            vec![Need {
                id,
                need_type: NeedType::Rel.directive().to_string(),
                title: format!("Release {} for {}", version, record.requirement_id),
                description: format!("Targets: {}", targets.join(", ")),
                status: "released".into(),
                links: vec![base_id],
                tags: vec!["release".into(), version.clone()],
                extra,
            }]
        }
    }
}

/// Generate sphinx-needs type configuration for conf.py.
pub fn needs_types_config() -> Vec<NeedTypeConfig> {
    vec![
        NeedTypeConfig {
            directive: "req".into(),
            title: "Requirement".into(),
            prefix: "REQ_".into(),
            color: "#BFD8D2".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "arch".into(),
            title: "Architecture".into(),
            prefix: "ARCH_".into(),
            color: "#DCFAC0".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "design".into(),
            title: "Detailed Design".into(),
            prefix: "DES_".into(),
            color: "#C0E0FA".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "impl".into(),
            title: "Implementation".into(),
            prefix: "IMPL_".into(),
            color: "#FED8B1".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "utest".into(),
            title: "Unit Test".into(),
            prefix: "UT_".into(),
            color: "#D5E8D4".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "itest".into(),
            title: "Integration Test".into(),
            prefix: "IT_".into(),
            color: "#DAE8FC".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "proof".into(),
            title: "Formal Proof".into(),
            prefix: "PRF_".into(),
            color: "#E1D5E7".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "review".into(),
            title: "Code Review".into(),
            prefix: "RVW_".into(),
            color: "#FFF2CC".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "verif".into(),
            title: "Verification Report".into(),
            prefix: "VER_".into(),
            color: "#F8CECC".into(),
            style: "node".into(),
        },
        NeedTypeConfig {
            directive: "rel".into(),
            title: "Release Artifact".into(),
            prefix: "REL_".into(),
            color: "#B0E0E6".into(),
            style: "node".into(),
        },
    ]
}

/// Configuration for a sphinx-needs type (used in conf.py generation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeedTypeConfig {
    pub directive: String,
    pub title: String,
    pub prefix: String,
    pub color: String,
    pub style: String,
}

/// Generate the `needs_types` Python list for conf.py.
pub fn generate_conf_py_needs_types() -> String {
    let types = needs_types_config();
    let mut lines = vec!["needs_types = [".to_string()];
    for t in &types {
        lines.push(format!(
            "    dict(directive=\"{}\", title=\"{}\", prefix=\"{}\", color=\"{}\", style=\"{}\"),",
            t.directive, t.title, t.prefix, t.color, t.style
        ));
    }
    lines.push("]".to_string());
    lines.join("\n")
}

/// Generate RST content for a traceability overview page.
pub fn generate_traceability_rst(tool_name: &str) -> String {
    let title = format!("{tool_name} Traceability");
    let underline = "=".repeat(title.len());
    format!(
        r#"{title}
{underline}

.. needimport:: needs.json

Requirements
------------
.. needlist::
   :types: req
   :style: table

Architecture
------------
.. needlist::
   :types: arch
   :style: table

Implementation
--------------
.. needlist::
   :types: impl
   :style: table

Unit Tests
----------
.. needlist::
   :types: utest
   :style: table

Formal Proofs
-------------
.. needlist::
   :types: proof
   :style: table

Integration Tests
-----------------
.. needlist::
   :types: itest
   :style: table

Reviews
-------
.. needlist::
   :types: review
   :style: table

Traceability Flow
-----------------
.. needflow::
   :filter: type in ['req', 'arch', 'impl', 'utest', 'proof', 'itest', 'review', 'rel']

Traceability Matrix
-------------------
.. needtable::
   :columns: id;title;type;status;links
   :style: table
"#,
    )
}

/// Sanitize a string for use as a sphinx-needs ID (alphanumeric + underscores).
fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_id_works() {
        assert_eq!(sanitize_id("REQ-LOOM-042"), "REQ_LOOM_042");
        assert_eq!(sanitize_id("some.thing/here"), "SOME_THING_HERE");
    }

    #[test]
    fn need_type_labels() {
        assert_eq!(NeedType::Req.label(), "Requirement");
        assert_eq!(NeedType::Proof.label(), "Formal Proof");
        assert_eq!(NeedType::Req.aspice_process(), "SWE.1");
        assert_eq!(NeedType::Itest.aspice_process(), "SWE.5");
    }

    #[test]
    fn needs_json_roundtrip() {
        let mut nj = NeedsJson::new("pulseengine", "0.1.0");
        nj.add(Need {
            id: "REQ_LOOM_042".into(),
            need_type: "req".into(),
            title: "Add i32.popcnt".into(),
            description: "Support i32.popcnt in the optimization pipeline".into(),
            status: "open".into(),
            links: vec![],
            tags: vec!["loom".into()],
            extra: HashMap::new(),
        });

        let json = nj.to_json().unwrap();
        let parsed: NeedsJson = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.needs.len(), 1);
        assert!(parsed.needs.contains_key("REQ_LOOM_042"));
    }

    #[test]
    fn trace_record_to_need_requirement() {
        let record = TraceRecord {
            id: 1,
            task_id: 42,
            requirement_id: "REQ-LOOM-042".into(),
            artifact: TraceArtifact::Requirement {
                title: "Add i32.popcnt".into(),
                description: "Support popcount".into(),
            },
            created_at: Utc::now(),
        };

        let needs = trace_record_to_needs(&record);
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].id, "REQ_LOOM_042");
        assert_eq!(needs[0].need_type, "req");
    }

    #[test]
    fn trace_record_to_need_impl() {
        let record = TraceRecord {
            id: 2,
            task_id: 42,
            requirement_id: "REQ-LOOM-042".into(),
            artifact: TraceArtifact::Implementation {
                branch: "auto/TASK-0042/loom/add-popcnt".into(),
                commit_sha: Some("abc123".into()),
                files_changed: vec!["src/lib.rs".into()],
            },
            created_at: Utc::now(),
        };

        let needs = trace_record_to_needs(&record);
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].need_type, "impl");
        // Links back to the requirement
        assert_eq!(needs[0].links, vec!["REQ_LOOM_042"]);
        assert_eq!(needs[0].extra.get("commit").unwrap(), "abc123");
    }

    #[test]
    fn conf_py_generation() {
        let conf = generate_conf_py_needs_types();
        assert!(conf.contains("needs_types = ["));
        assert!(conf.contains("directive=\"req\""));
        assert!(conf.contains("directive=\"proof\""));
        assert!(conf.contains("directive=\"rel\""));
    }

    #[test]
    fn traceability_rst_generation() {
        let rst = generate_traceability_rst("loom");
        assert!(rst.contains("needimport:: needs.json"));
        assert!(rst.contains("needflow::"));
        assert!(rst.contains("needtable::"));
    }
}
