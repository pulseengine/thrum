//! OpenTelemetry setup, audit trail configuration, and local trace storage.
//!
//! Bridges the `tracing` crate to OpenTelemetry for:
//! - Distributed tracing of AI invocations, gate runs, git operations
//! - Audit trail for functional safety certification (ISO 26262, IEC 62304)
//! - Export to OTLP collectors (Jaeger, Grafana Tempo) when available
//! - **Local file-based trace storage** (JSONL) for self-contained viewing
//!
//! Every traced operation becomes a span with structured attributes that
//! serve as machine-readable evidence for certification audits.

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Telemetry configuration.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Service name for OTel resource.
    pub service_name: String,
    /// OTLP endpoint (e.g., "http://localhost:4317").
    /// If None, no OTLP export (local-only).
    pub otlp_endpoint: Option<String>,
    /// Whether to output JSON-structured logs to console.
    pub json_logs: bool,
    /// Log level filter (e.g., "thrum=info,warn").
    pub log_filter: String,
    /// Directory for local JSONL trace files.
    /// If set, all spans and events are persisted here.
    pub trace_dir: Option<PathBuf>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "thrum".into(),
            otlp_endpoint: None,
            json_logs: false,
            log_filter: "thrum=info".into(),
            trace_dir: Some(PathBuf::from("traces")),
        }
    }
}

/// Initialize telemetry with optional OTLP export and local file storage.
///
/// Returns a guard that must be kept alive for the duration of the program.
/// Drop the guard to flush pending spans.
pub fn init_telemetry(config: &TelemetryConfig) -> Result<TelemetryGuard> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_filter));

    // --- OTLP provider (optional) ---
    let provider = if let Some(ref endpoint) = config.otlp_endpoint {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?;

        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .build();

        Some(provider)
    } else {
        None
    };

    // --- Build subscriber layers ---
    // Console layer (always present) — boxed to erase json vs plain type difference
    let console_layer = if config.json_logs {
        tracing_subscriber::fmt::layer()
            .json()
            .with_span_events(FmtSpan::NONE)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_span_events(FmtSpan::NONE)
            .boxed()
    };

    // File layer (local JSONL trace storage) — Option<Layer> is itself a Layer (no-op when None)
    let file_layer = if let Some(ref trace_dir) = config.trace_dir {
        std::fs::create_dir_all(trace_dir).context(format!(
            "failed to create trace dir: {}",
            trace_dir.display()
        ))?;

        let today = chrono::Utc::now().format("%Y-%m-%d");
        let trace_file_path = trace_dir.join(format!("spans-{today}.jsonl"));

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace_file_path)
            .context(format!(
                "failed to open trace file: {}",
                trace_file_path.display()
            ))?;

        Some(
            tracing_subscriber::fmt::layer()
                .json()
                .with_span_events(FmtSpan::CLOSE)
                .with_writer(Mutex::new(file))
                .with_ansi(false),
        )
    } else {
        None
    };

    // OTel layer (optional, only when OTLP endpoint configured)
    let otel_layer = provider.as_ref().map(|p| {
        let tracer = p.tracer(config.service_name.clone());
        tracing_opentelemetry::layer().with_tracer(tracer)
    });

    // Assemble: registry + filter + console + optional(file) + optional(otel)
    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .with(otel_layer)
        .init();

    Ok(TelemetryGuard { provider })
}

/// Guard that flushes OpenTelemetry spans on drop.
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(ref provider) = self.provider
            && let Err(e) = provider.shutdown()
        {
            eprintln!("failed to shut down OTel provider: {e}");
        }
    }
}

// ─── Local Trace Viewer ─────────────────────────────────────────────────

/// A stored trace event read from JSONL files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTraceEvent {
    /// ISO 8601 timestamp.
    pub timestamp: Option<String>,
    /// Log level (INFO, WARN, ERROR, DEBUG, TRACE).
    pub level: Option<String>,
    /// The message or event name.
    pub message: Option<String>,
    /// Structured fields on this event.
    #[serde(default)]
    pub fields: serde_json::Value,
    /// Target module path (e.g., "thrum_runner::claude").
    pub target: Option<String>,
    /// Current span info.
    pub span: Option<serde_json::Value>,
    /// Parent span chain.
    pub spans: Option<Vec<serde_json::Value>>,
}

impl std::fmt::Display for StoredTraceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ts = self.timestamp.as_deref().unwrap_or("?");
        let level = self.level.as_deref().unwrap_or("?");
        let target = self.target.as_deref().unwrap_or("");
        let msg = self.message.as_deref().unwrap_or("");

        // Shorten timestamp to HH:MM:SS
        let short_ts = if ts.len() >= 19 { &ts[11..19] } else { ts };

        write!(f, "{short_ts} {level:<5} {target:<40} {msg}")?;

        // Show key fields inline
        if let serde_json::Value::Object(ref map) = self.fields {
            let interesting: Vec<_> = map.iter().filter(|(k, _)| *k != "message").collect();
            if !interesting.is_empty() {
                write!(f, " |")?;
                for (k, v) in interesting {
                    write!(f, " {k}={v}")?;
                }
            }
        }

        Ok(())
    }
}

/// Reader for locally stored JSONL trace files.
pub struct TraceReader {
    trace_dir: PathBuf,
}

impl TraceReader {
    pub fn new(trace_dir: impl Into<PathBuf>) -> Self {
        Self {
            trace_dir: trace_dir.into(),
        }
    }

    /// List all trace files in the directory, sorted by date (newest first).
    pub fn trace_files(&self) -> Result<Vec<PathBuf>> {
        if !self.trace_dir.exists() {
            return Ok(Vec::new());
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.trace_dir)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "jsonl") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();
        files.sort();
        files.reverse(); // Newest first
        Ok(files)
    }

    /// Read events from all trace files, with optional filters.
    pub fn read_events(&self, filter: &TraceFilter) -> Result<Vec<StoredTraceEvent>> {
        let files = self.trace_files()?;
        let mut events = Vec::new();

        for file_path in &files {
            let file = std::fs::File::open(file_path)?;
            let reader = BufReader::new(file);

            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_str::<StoredTraceEvent>(&line)
                    && filter.matches(&event)
                {
                    events.push(event);
                }
            }

            if let Some(limit) = filter.limit
                && events.len() >= limit
            {
                events.truncate(limit);
                break;
            }
        }

        Ok(events)
    }

    /// Get a summary of stored traces.
    pub fn summary(&self) -> Result<TraceSummary> {
        let files = self.trace_files()?;
        let mut total_events = 0usize;
        let mut total_bytes = 0u64;

        for file_path in &files {
            let meta = std::fs::metadata(file_path)?;
            total_bytes += meta.len();

            let file = std::fs::File::open(file_path)?;
            total_events += BufReader::new(file).lines().count();
        }

        Ok(TraceSummary {
            file_count: files.len(),
            total_events,
            total_bytes,
            trace_dir: self.trace_dir.clone(),
            newest_file: files.first().cloned(),
        })
    }
}

/// Filter for querying stored trace events.
#[derive(Debug, Default)]
pub struct TraceFilter {
    /// Max number of events to return.
    pub limit: Option<usize>,
    /// Filter by log level (e.g., "INFO", "WARN").
    pub level: Option<String>,
    /// Filter by target module prefix (e.g., "thrum_runner").
    pub target_prefix: Option<String>,
    /// Filter by field key=value (e.g., "task.id=42").
    pub field_filter: Option<(String, String)>,
}

impl TraceFilter {
    fn matches(&self, event: &StoredTraceEvent) -> bool {
        if let Some(ref level) = self.level
            && event.level.as_deref() != Some(level.as_str())
        {
            return false;
        }
        if let Some(ref prefix) = self.target_prefix
            && !event
                .target
                .as_deref()
                .is_some_and(|t| t.starts_with(prefix.as_str()))
        {
            return false;
        }
        if let Some((ref key, ref value)) = self.field_filter {
            if let serde_json::Value::Object(ref map) = event.fields {
                if let Some(field_val) = map.get(key.as_str()) {
                    let field_str = match field_val {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if field_str != *value {
                        return false;
                    }
                } else {
                    return false;
                }
            } else {
                return false;
            }
        }
        true
    }
}

/// Summary info about stored traces.
#[derive(Debug)]
pub struct TraceSummary {
    pub file_count: usize,
    pub total_events: usize,
    pub total_bytes: u64,
    pub trace_dir: PathBuf,
    pub newest_file: Option<PathBuf>,
}

impl std::fmt::Display for TraceSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Trace directory: {}", self.trace_dir.display())?;
        writeln!(f, "Files:           {}", self.file_count)?;
        writeln!(f, "Total events:    {}", self.total_events)?;
        writeln!(
            f,
            "Total size:      {:.1} KB",
            self.total_bytes as f64 / 1024.0
        )?;
        if let Some(ref newest) = self.newest_file {
            writeln!(
                f,
                "Newest file:     {}",
                newest.file_name().unwrap_or_default().to_string_lossy()
            )?;
        }
        Ok(())
    }
}

/// Standard span attribute names for audit trail.
/// Use these as keys in `tracing::info_span!()` for consistency.
pub mod attrs {
    /// Task identifier (e.g., "TASK-0042").
    pub const TASK_ID: &str = "task.id";
    /// Repository name (e.g., "loom").
    pub const REPO: &str = "repo";
    /// Gate level (e.g., "quality", "proof", "integration").
    pub const GATE_LEVEL: &str = "gate.level";
    /// Whether a gate passed.
    pub const GATE_PASSED: &str = "gate.passed";
    /// AI model used.
    pub const AI_MODEL: &str = "ai.model";
    /// AI backend name.
    pub const AI_BACKEND: &str = "ai.backend";
    /// Input tokens consumed.
    pub const AI_INPUT_TOKENS: &str = "ai.input_tokens";
    /// Output tokens produced.
    pub const AI_OUTPUT_TOKENS: &str = "ai.output_tokens";
    /// Estimated cost in USD.
    pub const AI_COST_USD: &str = "ai.cost_usd";
    /// Git branch name.
    pub const GIT_BRANCH: &str = "git.branch";
    /// Git commit SHA.
    pub const GIT_COMMIT: &str = "git.commit";
    /// Pipeline stage (e.g., "implement", "review", "gate1").
    pub const PIPELINE_STAGE: &str = "pipeline.stage";
    /// Safety classification (e.g., "ASIL B").
    pub const SAFETY_ASIL: &str = "safety.asil";
    /// Tool confidence level.
    pub const SAFETY_TCL: &str = "safety.tcl";
    /// Requirement ID for traceability.
    pub const REQUIREMENT_ID: &str = "requirement.id";
    /// Check name within a gate.
    pub const CHECK_NAME: &str = "check.name";
    /// Check result (pass/fail).
    pub const CHECK_PASSED: &str = "check.passed";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_filter_matches_level() {
        let event = StoredTraceEvent {
            timestamp: Some("2025-01-01T00:00:00Z".into()),
            level: Some("INFO".into()),
            message: Some("test".into()),
            fields: serde_json::Value::Object(Default::default()),
            target: Some("thrum".into()),
            span: None,
            spans: None,
        };

        let filter = TraceFilter {
            level: Some("INFO".into()),
            ..Default::default()
        };
        assert!(filter.matches(&event));

        let filter = TraceFilter {
            level: Some("WARN".into()),
            ..Default::default()
        };
        assert!(!filter.matches(&event));
    }

    #[test]
    fn trace_filter_matches_target() {
        let event = StoredTraceEvent {
            timestamp: None,
            level: None,
            message: None,
            fields: serde_json::Value::Object(Default::default()),
            target: Some("thrum_runner::claude".into()),
            span: None,
            spans: None,
        };

        let filter = TraceFilter {
            target_prefix: Some("thrum_runner".into()),
            ..Default::default()
        };
        assert!(filter.matches(&event));

        let filter = TraceFilter {
            target_prefix: Some("thrum_core".into()),
            ..Default::default()
        };
        assert!(!filter.matches(&event));
    }

    #[test]
    fn stored_event_display() {
        let event = StoredTraceEvent {
            timestamp: Some("2025-06-15T10:30:45.123Z".into()),
            level: Some("INFO".into()),
            message: Some("invoking claude CLI".into()),
            fields: serde_json::json!({"prompt_len": 1234}),
            target: Some("thrum_runner::claude".into()),
            span: None,
            spans: None,
        };
        let display = format!("{event}");
        assert!(display.contains("10:30:45"));
        assert!(display.contains("invoking claude CLI"));
        assert!(display.contains("prompt_len"));
    }
}
