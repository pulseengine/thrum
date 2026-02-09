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
}
