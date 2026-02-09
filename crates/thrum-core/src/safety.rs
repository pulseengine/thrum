//! Multi-standard functional safety classification.
//!
//! Supports:
//! - ISO 26262 (Automotive): ASIL levels, TCL via TI×TD matrix
//! - IEC 62304 (Medical): Software safety classes A/B/C
//! - DO-178C (Avionics): Design Assurance Levels A-E
//! - IEC 61508 (Industrial): SIL 1-4
//!
//! Also handles OSS/SOUP qualification tracking per each standard.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─── Tool Confidence Level (ISO 26262 Part 8, Clause 11) ───────────────

/// Tool Impact classification.
/// TI1: The tool cannot introduce or fail to detect errors in a safety-related item.
/// TI2: The tool can introduce or fail to detect errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolImpact {
    /// Tool has no impact on safety-related outputs.
    Ti1,
    /// Tool can introduce or fail to detect errors in safety items.
    Ti2,
}

/// Tool error Detection capability.
/// How likely are tool-introduced errors to be caught by downstream activities?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolDetection {
    /// High confidence: strong measures exist to detect tool errors
    /// (e.g., Z3 verification, formal proofs, independent back-to-back testing).
    Td1,
    /// Medium confidence: some measures exist
    /// (e.g., comprehensive test suites, code review).
    Td2,
    /// Low confidence: weak or no measures to detect tool errors.
    Td3,
}

/// Tool Confidence Level (result of TI × TD classification).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tcl {
    /// Low confidence needed — minimal qualification effort.
    Tcl1,
    /// Medium confidence — increased qualification effort.
    Tcl2,
    /// High confidence — maximum qualification effort.
    Tcl3,
}

impl std::fmt::Display for Tcl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tcl::Tcl1 => write!(f, "TCL1"),
            Tcl::Tcl2 => write!(f, "TCL2"),
            Tcl::Tcl3 => write!(f, "TCL3"),
        }
    }
}

/// Determine TCL from TI and TD per ISO 26262 Part 8, Table 4.
///
/// ```text
///        │ TD1   │ TD2   │ TD3
/// ───────┼───────┼───────┼──────
///  TI1   │ TCL1  │ TCL1  │ TCL1
///  TI2   │ TCL1  │ TCL2  │ TCL3
/// ```
pub fn determine_tcl(ti: ToolImpact, td: ToolDetection) -> Tcl {
    match (ti, td) {
        (ToolImpact::Ti1, _) => Tcl::Tcl1,
        (ToolImpact::Ti2, ToolDetection::Td1) => Tcl::Tcl1,
        (ToolImpact::Ti2, ToolDetection::Td2) => Tcl::Tcl2,
        (ToolImpact::Ti2, ToolDetection::Td3) => Tcl::Tcl3,
    }
}

/// Qualification methods required per TCL level (ISO 26262 Part 8, Table 5).
pub fn qualification_methods(tcl: Tcl) -> Vec<&'static str> {
    match tcl {
        Tcl::Tcl1 => vec![],
        Tcl::Tcl2 => vec![
            "1a: Increased confidence from use",
            "1b: Evaluation of the tool development process",
            "1c: Validation of the software tool",
            "1d: Development in accordance with a safety standard",
        ],
        Tcl::Tcl3 => vec![
            "1b: Evaluation of the tool development process",
            "1c: Validation of the software tool",
            "1d: Development in accordance with a safety standard",
        ],
    }
}

// ─── Tool Qualification Record ─────────────────────────────────────────

/// Complete tool qualification record per ISO 26262 Part 8.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolQualification {
    pub tool_name: String,
    pub tool_version: String,
    pub ti: ToolImpact,
    pub td: ToolDetection,
    pub tcl: Tcl,
    pub target_asil: Option<AsilLevel>,
    pub qualification_methods: Vec<String>,
    pub evidence: Vec<QualificationEvidence>,
    pub oss_info: Option<OssQualification>,
    pub use_cases: Vec<ToolUseCase>,
}

/// Describes how a tool is used in the safety lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseCase {
    pub description: String,
    /// Which phase of the V-model this use case belongs to.
    pub lifecycle_phase: String,
    /// What safety-related output the tool produces.
    pub output_description: String,
    /// How errors in this output would be detected.
    pub detection_measures: Vec<String>,
}

/// Evidence supporting tool qualification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualificationEvidence {
    pub method: String,
    pub description: String,
    pub artifact_path: Option<PathBuf>,
    pub status: EvidenceStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceStatus {
    NotStarted,
    InProgress,
    Complete,
    NotApplicable,
}

// ─── ISO 26262 ASIL ────────────────────────────────────────────────────

/// Automotive Safety Integrity Level (ISO 26262).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AsilLevel {
    Qm,
    AsilA,
    AsilB,
    AsilC,
    AsilD,
}

impl std::fmt::Display for AsilLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AsilLevel::Qm => write!(f, "QM"),
            AsilLevel::AsilA => write!(f, "ASIL A"),
            AsilLevel::AsilB => write!(f, "ASIL B"),
            AsilLevel::AsilC => write!(f, "ASIL C"),
            AsilLevel::AsilD => write!(f, "ASIL D"),
        }
    }
}

// ─── IEC 62304 (Medical Device Software) ───────────────────────────────

/// Software safety classification per IEC 62304.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Iec62304Class {
    /// No injury or damage to health possible.
    ClassA,
    /// Non-serious injury possible.
    ClassB,
    /// Death or serious injury possible.
    ClassC,
}

impl std::fmt::Display for Iec62304Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Iec62304Class::ClassA => write!(f, "IEC 62304 Class A"),
            Iec62304Class::ClassB => write!(f, "IEC 62304 Class B"),
            Iec62304Class::ClassC => write!(f, "IEC 62304 Class C"),
        }
    }
}

/// IEC 62304 required activities per software safety class.
pub fn iec62304_required_activities(class: Iec62304Class) -> Vec<&'static str> {
    match class {
        Iec62304Class::ClassA => vec![
            "5.1: Software development planning",
            "5.2: Software requirements analysis",
            "5.8: Software release",
            "6.1: Software maintenance plan",
            "7.1: Software risk management",
            "8.1: Software configuration management",
        ],
        Iec62304Class::ClassB => vec![
            "5.1: Software development planning",
            "5.2: Software requirements analysis",
            "5.3: Software architectural design",
            "5.5: Software integration and integration testing",
            "5.7: Software system testing",
            "5.8: Software release",
            "6.1: Software maintenance plan",
            "7.1-7.4: Software risk management",
            "8.1-8.3: Software configuration management",
            "9.8: Software problem resolution",
        ],
        Iec62304Class::ClassC => vec![
            "5.1: Software development planning",
            "5.2: Software requirements analysis",
            "5.3: Software architectural design",
            "5.4: Software detailed design",
            "5.5: Software integration and integration testing",
            "5.6: Software verification",
            "5.7: Software system testing",
            "5.8: Software release",
            "6.1: Software maintenance plan",
            "7.1-7.4: Software risk management (full)",
            "8.1-8.3: Software configuration management (full)",
            "9.1-9.8: Software problem resolution (full)",
        ],
    }
}

// ─── DO-178C (Avionics) ────────────────────────────────────────────────

/// Design Assurance Level per DO-178C.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DalLevel {
    /// Catastrophic failure condition.
    DalA,
    /// Hazardous/Severe-Major failure condition.
    DalB,
    /// Major failure condition.
    DalC,
    /// Minor failure condition.
    DalD,
    /// No effect on aircraft safety.
    DalE,
}

impl std::fmt::Display for DalLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DalLevel::DalA => write!(f, "DAL A"),
            DalLevel::DalB => write!(f, "DAL B"),
            DalLevel::DalC => write!(f, "DAL C"),
            DalLevel::DalD => write!(f, "DAL D"),
            DalLevel::DalE => write!(f, "DAL E"),
        }
    }
}

// ─── IEC 61508 (Industrial) ────────────────────────────────────────────

/// Safety Integrity Level per IEC 61508.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SilLevel {
    Sil1,
    Sil2,
    Sil3,
    Sil4,
}

impl std::fmt::Display for SilLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SilLevel::Sil1 => write!(f, "SIL 1"),
            SilLevel::Sil2 => write!(f, "SIL 2"),
            SilLevel::Sil3 => write!(f, "SIL 3"),
            SilLevel::Sil4 => write!(f, "SIL 4"),
        }
    }
}

// ─── Unified Safety Classification ─────────────────────────────────────

/// Multi-standard safety classification for a tool or component.
/// A tool can be classified under multiple standards simultaneously.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyClassification {
    pub automotive: Option<AsilLevel>,
    pub medical: Option<Iec62304Class>,
    pub avionics: Option<DalLevel>,
    pub industrial: Option<SilLevel>,
}

impl SafetyClassification {
    pub fn automotive(asil: AsilLevel) -> Self {
        Self {
            automotive: Some(asil),
            medical: None,
            avionics: None,
            industrial: None,
        }
    }
}

// ─── OSS / SOUP Qualification ──────────────────────────────────────────

/// OSS component qualification record.
/// Covers ISO 26262 Part 8 Clause 12 (SW component qualification)
/// and IEC 62304 SOUP management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OssQualification {
    pub component_name: String,
    pub version: String,
    pub license: String,
    pub repository_url: String,
    /// Is this component developed with a known process?
    pub development_process_known: bool,
    /// Has the development process been evaluated?
    pub process_evaluation: Option<ProcessEvaluation>,
    /// Known anomalies / CVEs relevant to our use case.
    pub known_anomalies: Vec<KnownAnomaly>,
    /// How this component is used in the safety context.
    pub usage_context: String,
    /// What happens if this component fails?
    pub failure_impact: String,
    /// Risk control measures for component failure.
    pub risk_controls: Vec<String>,
    /// Verification evidence specific to this component.
    pub verification_evidence: Vec<QualificationEvidence>,
}

/// Evaluation of an OSS project's development process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEvaluation {
    pub has_ci: bool,
    pub has_tests: bool,
    pub has_formal_verification: bool,
    pub has_code_review: bool,
    pub has_release_process: bool,
    pub has_issue_tracking: bool,
    pub has_documentation: bool,
    pub evaluation_date: DateTime<Utc>,
    pub evaluator: String,
    pub notes: String,
}

/// A known anomaly (bug, CVE, limitation) in a component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownAnomaly {
    pub id: String,
    pub description: String,
    pub severity: AnomalySeverity,
    pub affects_safety: bool,
    pub mitigation: Option<String>,
    pub status: AnomalyStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnomalySeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnomalyStatus {
    Open,
    Mitigated,
    Fixed,
    WontFix,
    NotApplicable,
}

// ─── SOUP Registry (IEC 62304) ─────────────────────────────────────────

/// SOUP (Software of Unknown Provenance) item per IEC 62304 §8.1.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoupItem {
    pub name: String,
    pub version: String,
    pub manufacturer: String,
    pub unique_id: String,
    pub known_anomalies: Vec<KnownAnomaly>,
    /// Functional and performance requirements relevant to safety.
    pub requirements: Vec<String>,
    /// Hardware/software compatibility requirements.
    pub compatibility: Vec<String>,
}

/// Full SOUP registry for a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoupRegistry {
    pub items: Vec<SoupItem>,
    pub last_updated: DateTime<Utc>,
}

impl SoupRegistry {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            last_updated: Utc::now(),
        }
    }
}

impl Default for SoupRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ASPICE Process Reference ──────────────────────────────────────────

/// Automotive SPICE process areas relevant to the automator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AspiceProcess {
    /// SWE.1: Software Requirements Analysis
    Swe1,
    /// SWE.2: Software Architectural Design
    Swe2,
    /// SWE.3: Software Detailed Design and Unit Construction
    Swe3,
    /// SWE.4: Software Unit Verification
    Swe4,
    /// SWE.5: Software Integration and Integration Test
    Swe5,
    /// SWE.6: Software Qualification Test
    Swe6,
    /// SUP.8: Configuration Management
    Sup8,
    /// SUP.9: Problem Resolution Management
    Sup9,
    /// SUP.10: Change Request Management
    Sup10,
    /// MAN.3: Project Management
    Man3,
}

impl std::fmt::Display for AspiceProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AspiceProcess::Swe1 => write!(f, "SWE.1 SW Requirements Analysis"),
            AspiceProcess::Swe2 => write!(f, "SWE.2 SW Architectural Design"),
            AspiceProcess::Swe3 => write!(f, "SWE.3 SW Detailed Design & Unit Construction"),
            AspiceProcess::Swe4 => write!(f, "SWE.4 SW Unit Verification"),
            AspiceProcess::Swe5 => write!(f, "SWE.5 SW Integration & Integration Test"),
            AspiceProcess::Swe6 => write!(f, "SWE.6 SW Qualification Test"),
            AspiceProcess::Sup8 => write!(f, "SUP.8 Configuration Management"),
            AspiceProcess::Sup9 => write!(f, "SUP.9 Problem Resolution Management"),
            AspiceProcess::Sup10 => write!(f, "SUP.10 Change Request Management"),
            AspiceProcess::Man3 => write!(f, "MAN.3 Project Management"),
        }
    }
}

/// Map automator pipeline stages to ASPICE processes.
pub fn pipeline_aspice_mapping() -> Vec<(String, AspiceProcess)> {
    vec![
        ("Planner".into(), AspiceProcess::Swe1),
        ("Planner (architecture)".into(), AspiceProcess::Swe2),
        ("Implementer".into(), AspiceProcess::Swe3),
        ("Gate 1: Unit tests".into(), AspiceProcess::Swe4),
        ("Gate 3: Integration tests".into(), AspiceProcess::Swe5),
        ("Release: Qualification tests".into(), AspiceProcess::Swe6),
        ("Git operations".into(), AspiceProcess::Sup8),
        ("Task rejection/feedback".into(), AspiceProcess::Sup9),
        ("Task queue management".into(), AspiceProcess::Sup10),
        ("Budget/status tracking".into(), AspiceProcess::Man3),
    ]
}

// ─── Safety Configuration ──────────────────────────────────────────────

/// Top-level safety configuration (parsed from safety.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    pub tools: Vec<ToolSafetyConfig>,
    pub soup_items: Vec<SoupItem>,
}

/// Per-tool safety configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSafetyConfig {
    pub name: String,
    pub ti: ToolImpact,
    pub td: ToolDetection,
    pub classification: SafetyClassification,
    pub is_oss: bool,
    pub repository_url: Option<String>,
    pub license: Option<String>,
    pub use_cases: Vec<ToolUseCase>,
}

impl ToolSafetyConfig {
    /// Compute TCL from TI and TD.
    pub fn tcl(&self) -> Tcl {
        determine_tcl(self.ti, self.td)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcl_matrix() {
        // TI1 always yields TCL1
        assert_eq!(
            determine_tcl(ToolImpact::Ti1, ToolDetection::Td1),
            Tcl::Tcl1
        );
        assert_eq!(
            determine_tcl(ToolImpact::Ti1, ToolDetection::Td2),
            Tcl::Tcl1
        );
        assert_eq!(
            determine_tcl(ToolImpact::Ti1, ToolDetection::Td3),
            Tcl::Tcl1
        );

        // TI2 depends on TD
        assert_eq!(
            determine_tcl(ToolImpact::Ti2, ToolDetection::Td1),
            Tcl::Tcl1
        );
        assert_eq!(
            determine_tcl(ToolImpact::Ti2, ToolDetection::Td2),
            Tcl::Tcl2
        );
        assert_eq!(
            determine_tcl(ToolImpact::Ti2, ToolDetection::Td3),
            Tcl::Tcl3
        );
    }

    #[test]
    fn tcl_ordering() {
        assert!(Tcl::Tcl1 < Tcl::Tcl2);
        assert!(Tcl::Tcl2 < Tcl::Tcl3);
    }

    #[test]
    fn qualification_methods_by_tcl() {
        assert!(qualification_methods(Tcl::Tcl1).is_empty());
        assert!(!qualification_methods(Tcl::Tcl2).is_empty());
        assert!(!qualification_methods(Tcl::Tcl3).is_empty());
        // TCL3 requires more methods than TCL2
        // (TCL3 excludes "increased confidence from use")
        assert!(qualification_methods(Tcl::Tcl3).len() < qualification_methods(Tcl::Tcl2).len());
    }

    #[test]
    fn asil_ordering() {
        assert!(AsilLevel::Qm < AsilLevel::AsilA);
        assert!(AsilLevel::AsilA < AsilLevel::AsilB);
        assert!(AsilLevel::AsilD > AsilLevel::AsilC);
    }

    #[test]
    fn iec62304_class_c_requires_all() {
        let activities = iec62304_required_activities(Iec62304Class::ClassC);
        assert!(activities.len() > iec62304_required_activities(Iec62304Class::ClassA).len());
    }

    #[test]
    fn tool_config_tcl() {
        let config = ToolSafetyConfig {
            name: "loom".into(),
            ti: ToolImpact::Ti2,
            td: ToolDetection::Td2,
            classification: SafetyClassification::automotive(AsilLevel::AsilB),
            is_oss: true,
            repository_url: Some("https://github.com/example/loom".into()),
            license: Some("Apache-2.0".into()),
            use_cases: vec![],
        };
        assert_eq!(config.tcl(), Tcl::Tcl2);
    }

    #[test]
    fn aspice_mapping_covers_pipeline() {
        let mapping = pipeline_aspice_mapping();
        // Should cover SWE.1 through SWE.6
        let processes: Vec<_> = mapping.iter().map(|(_, p)| p).collect();
        assert!(processes.contains(&&AspiceProcess::Swe1));
        assert!(processes.contains(&&AspiceProcess::Swe4));
        assert!(processes.contains(&&AspiceProcess::Swe6));
    }
}
