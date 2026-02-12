use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Token/cost budget tracking for AI sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetTracker {
    pub entries: Vec<BudgetEntry>,
    pub ceiling_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetEntry {
    pub task_id: i64,
    pub session_type: SessionType,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_usd: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionType {
    Planning,
    Implementation,
    Review,
    Integration,
}

impl BudgetTracker {
    pub fn new(ceiling_usd: f64) -> Self {
        Self {
            entries: Vec::new(),
            ceiling_usd,
        }
    }

    pub fn total_spent(&self) -> f64 {
        self.entries.iter().map(|e| e.estimated_cost_usd).sum()
    }

    pub fn remaining(&self) -> f64 {
        self.ceiling_usd - self.total_spent()
    }

    pub fn is_over_budget(&self) -> bool {
        self.total_spent() >= self.ceiling_usd
    }

    pub fn record(&mut self, entry: BudgetEntry) {
        self.entries.push(entry);
    }

    /// Check if executing a role with the given per-invocation budget would
    /// exceed the ceiling. Returns `true` if there is enough remaining budget.
    pub fn can_afford(&self, role_budget_usd: f64) -> bool {
        self.remaining() >= role_budget_usd
    }

    /// Breakdown by session type.
    pub fn by_session_type(&self) -> Vec<(SessionType, f64)> {
        let mut planning = 0.0;
        let mut implementation = 0.0;
        let mut review = 0.0;
        let mut integration = 0.0;

        for entry in &self.entries {
            match entry.session_type {
                SessionType::Planning => planning += entry.estimated_cost_usd,
                SessionType::Implementation => implementation += entry.estimated_cost_usd,
                SessionType::Review => review += entry.estimated_cost_usd,
                SessionType::Integration => integration += entry.estimated_cost_usd,
            }
        }

        vec![
            (SessionType::Planning, planning),
            (SessionType::Implementation, implementation),
            (SessionType::Review, review),
            (SessionType::Integration, integration),
        ]
    }
}

/// Estimate cost in USD from token counts and model name.
///
/// Uses approximate per-million-token pricing:
/// - Opus models: $15/M input, $75/M output
/// - Sonnet models: $3/M input, $15/M output
/// - Haiku models: $0.25/M input, $1.25/M output
/// - Other/unknown: $3/M input, $15/M output (Sonnet-equivalent)
///
/// If token counts are unavailable, falls back to `fallback_usd`.
pub fn estimate_cost(
    model: &str,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    fallback_usd: f64,
) -> f64 {
    let (input, output) = match (input_tokens, output_tokens) {
        (Some(i), Some(o)) => (i, o),
        _ => return fallback_usd,
    };

    let model_lower = model.to_lowercase();
    let (input_rate, output_rate) = if model_lower.contains("opus") {
        (15.0, 75.0) // per million tokens
    } else if model_lower.contains("haiku") {
        (0.25, 1.25)
    } else {
        // Sonnet-equivalent default
        (3.0, 15.0)
    };

    let cost =
        (input as f64 * input_rate / 1_000_000.0) + (output as f64 * output_rate / 1_000_000.0);

    // Use at least a minimal floor to avoid zero-cost entries
    cost.max(0.001)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_tracking() {
        let mut tracker = BudgetTracker::new(10_000.0);
        assert_eq!(tracker.remaining(), 10_000.0);
        assert!(!tracker.is_over_budget());

        tracker.record(BudgetEntry {
            task_id: 1,
            session_type: SessionType::Implementation,
            model: "opus-4".into(),
            input_tokens: 50_000,
            output_tokens: 10_000,
            estimated_cost_usd: 15.0,
            timestamp: Utc::now(),
        });

        assert_eq!(tracker.total_spent(), 15.0);
        assert_eq!(tracker.remaining(), 9_985.0);
    }

    #[test]
    fn can_afford_check() {
        let mut tracker = BudgetTracker::new(10.0);
        assert!(tracker.can_afford(6.0));
        assert!(tracker.can_afford(10.0));
        assert!(!tracker.can_afford(10.01));

        tracker.record(BudgetEntry {
            task_id: 1,
            session_type: SessionType::Implementation,
            model: "opus-4".into(),
            input_tokens: 0,
            output_tokens: 0,
            estimated_cost_usd: 7.0,
            timestamp: Utc::now(),
        });

        assert!(tracker.can_afford(3.0));
        assert!(!tracker.can_afford(3.01));
    }

    #[test]
    fn estimate_cost_opus() {
        // 100k input, 10k output on Opus
        // = (100_000 * 15 / 1M) + (10_000 * 75 / 1M)
        // = 1.5 + 0.75 = 2.25
        let cost = estimate_cost("claude-opus-4-6", Some(100_000), Some(10_000), 5.0);
        assert!((cost - 2.25).abs() < 0.001);
    }

    #[test]
    fn estimate_cost_sonnet() {
        // 100k input, 10k output on Sonnet
        // = (100_000 * 3 / 1M) + (10_000 * 15 / 1M)
        // = 0.3 + 0.15 = 0.45
        let cost = estimate_cost("claude-sonnet-4-5", Some(100_000), Some(10_000), 5.0);
        assert!((cost - 0.45).abs() < 0.001);
    }

    #[test]
    fn estimate_cost_fallback_when_no_tokens() {
        let cost = estimate_cost("opus-4", None, None, 6.0);
        assert!((cost - 6.0).abs() < f64::EPSILON);
    }
}
