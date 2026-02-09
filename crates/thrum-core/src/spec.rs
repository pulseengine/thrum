//! Structured specification format for spec-driven development (SDD).
//!
//! Aligns with the SDD methodology: specifications are the source of truth,
//! AI agents generate code against them, and gates verify conformance.
//!
//! Inspired by GitHub Spec Kit's Markdown approach but extended with:
//! - Formal proof obligations (for Z3/Rocq gates)
//! - Safety relevance annotations (for ISO 26262 / IEC 62304)
//! - Structured design decisions (not just free text)

use serde::{Deserialize, Serialize};

/// A structured specification for a development task.
///
/// This is the "contract" that AI-generated code must satisfy.
/// Gates verify conformance; the spec is the audit artifact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Spec {
    /// One-line summary of what this spec defines.
    pub title: String,
    /// Why this change is needed — business/safety/technical context.
    pub context: String,
    /// Structured requirements with IDs for traceability.
    #[serde(default)]
    pub requirements: Vec<SpecRequirement>,
    /// Design approach and constraints.
    pub design: DesignSpec,
    /// Concrete conditions that must be true when implementation is complete.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// What must be formally proven (Z3/Rocq).
    #[serde(default)]
    pub proof_obligations: Vec<ProofObligation>,
    /// How to verify beyond automated gates (manual testing, edge cases).
    #[serde(default)]
    pub test_plan: Vec<String>,
}

/// A traceable requirement within a spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecRequirement {
    /// Unique ID for traceability (e.g., "REQ-LOOM-042").
    pub id: String,
    /// What the requirement states.
    pub description: String,
    /// Why this requirement exists.
    #[serde(default)]
    pub rationale: String,
    /// Priority level.
    #[serde(default)]
    pub priority: Priority,
    /// Safety relevance annotation (e.g., "ASIL B — translation must preserve semantics").
    #[serde(default)]
    pub safety_relevance: Option<String>,
}

/// Design decisions and constraints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesignSpec {
    /// High-level approach description.
    #[serde(default)]
    pub approach: String,
    /// Files expected to be modified or created.
    #[serde(default)]
    pub affected_files: Vec<String>,
    /// Interface contracts (function signatures, types).
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// Hard constraints (performance, compatibility, safety).
    #[serde(default)]
    pub constraints: Vec<String>,
}

/// A formal proof obligation that Gate 2 will verify.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofObligation {
    /// What property must be proven.
    pub property: String,
    /// Which prover (Z3, Rocq, Coq).
    #[serde(default)]
    pub prover: String,
    /// File where the proof should live.
    #[serde(default)]
    pub proof_file: Option<String>,
}

/// Priority levels for requirements.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    /// Blocking: must be done before anything else.
    P0,
    /// High: safety-critical or integration-blocking.
    #[default]
    P1,
    /// Medium: feature development per roadmap.
    P2,
    /// Low: quality improvements, documentation.
    P3,
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Priority::P0 => write!(f, "P0"),
            Priority::P1 => write!(f, "P1"),
            Priority::P2 => write!(f, "P2"),
            Priority::P3 => write!(f, "P3"),
        }
    }
}

impl Spec {
    /// Generate a Markdown representation of the spec (for agent prompts).
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str(&format!("# {}\n\n", self.title));

        if !self.context.is_empty() {
            md.push_str(&format!("## Context\n\n{}\n\n", self.context));
        }

        if !self.requirements.is_empty() {
            md.push_str("## Requirements\n\n");
            for req in &self.requirements {
                md.push_str(&format!(
                    "- **{}** [{}]: {}\n",
                    req.id, req.priority, req.description
                ));
                if let Some(ref safety) = req.safety_relevance {
                    md.push_str(&format!("  - Safety: {safety}\n"));
                }
            }
            md.push('\n');
        }

        if !self.design.approach.is_empty() {
            md.push_str("## Design\n\n");
            md.push_str(&format!("{}\n\n", self.design.approach));
            if !self.design.affected_files.is_empty() {
                md.push_str("### Affected Files\n\n");
                for f in &self.design.affected_files {
                    md.push_str(&format!("- `{f}`\n"));
                }
                md.push('\n');
            }
            if !self.design.constraints.is_empty() {
                md.push_str("### Constraints\n\n");
                for c in &self.design.constraints {
                    md.push_str(&format!("- {c}\n"));
                }
                md.push('\n');
            }
        }

        if !self.acceptance_criteria.is_empty() {
            md.push_str("## Acceptance Criteria\n\n");
            for ac in &self.acceptance_criteria {
                md.push_str(&format!("- [ ] {ac}\n"));
            }
            md.push('\n');
        }

        if !self.proof_obligations.is_empty() {
            md.push_str("## Proof Obligations\n\n");
            for po in &self.proof_obligations {
                md.push_str(&format!("- **{}**: {}\n", po.prover, po.property));
                if let Some(ref pf) = po.proof_file {
                    md.push_str(&format!("  - File: `{pf}`\n"));
                }
            }
            md.push('\n');
        }

        if !self.test_plan.is_empty() {
            md.push_str("## Test Plan\n\n");
            for tp in &self.test_plan {
                md.push_str(&format!("- {tp}\n"));
            }
        }

        md
    }

    /// Parse a spec from TOML (for spec files in the repo).
    pub fn from_toml(content: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(content)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_roundtrip_json() {
        let spec = Spec {
            title: "Add i32.popcnt to ISLE pipeline".into(),
            context: "Required for WASM spec compliance".into(),
            requirements: vec![SpecRequirement {
                id: "REQ-LOOM-042".into(),
                description: "Implement i32.popcnt instruction".into(),
                rationale: "WASM MVP spec requirement".into(),
                priority: Priority::P1,
                safety_relevance: Some("ASIL B — must preserve semantics".into()),
            }],
            design: DesignSpec {
                approach: "Add to 9 ISLE locations per CLAUDE.md checklist".into(),
                affected_files: vec!["src/isle/lower.rs".into()],
                interfaces: vec!["fn emit_popcnt(dst: Reg, src: Reg)".into()],
                constraints: vec!["Must not regress existing instruction lowering".into()],
            },
            acceptance_criteria: vec![
                "cargo test passes with new popcnt tests".into(),
                "Z3 translation validation proof added".into(),
            ],
            proof_obligations: vec![ProofObligation {
                property: "popcnt(x) == popcount(x) for all i32 values".into(),
                prover: "Z3".into(),
                proof_file: Some("proofs/popcnt.z3".into()),
            }],
            test_plan: vec!["Test all edge cases: 0, 1, MAX_U32, alternating bits".into()],
        };

        let json = serde_json::to_string_pretty(&spec).unwrap();
        let parsed: Spec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, spec.title);
        assert_eq!(parsed.requirements.len(), 1);
    }

    #[test]
    fn spec_to_markdown() {
        let spec = Spec {
            title: "Test spec".into(),
            context: "Testing markdown generation".into(),
            requirements: vec![SpecRequirement {
                id: "REQ-001".into(),
                description: "Must work".into(),
                rationale: String::new(),
                priority: Priority::P0,
                safety_relevance: None,
            }],
            design: DesignSpec::default(),
            acceptance_criteria: vec!["Tests pass".into()],
            proof_obligations: Vec::new(),
            test_plan: Vec::new(),
        };
        let md = spec.to_markdown();
        assert!(md.contains("# Test spec"));
        assert!(md.contains("REQ-001"));
        assert!(md.contains("- [ ] Tests pass"));
    }

    #[test]
    fn priority_display() {
        assert_eq!(format!("{}", Priority::P0), "P0");
        assert_eq!(format!("{}", Priority::P3), "P3");
    }
}
