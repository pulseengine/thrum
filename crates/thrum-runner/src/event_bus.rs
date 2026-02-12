//! Broadcast-based event bus for pipeline observability.
//!
//! Wraps `tokio::sync::broadcast` so multiple consumers (TUI, SSE, logger)
//! each receive their own copy of every event. Slow consumers are dropped
//! gracefully via the channel's lag mechanism.

use std::sync::Arc;
use thrum_core::event::{EventKind, PipelineEvent};
use tokio::sync::broadcast;

/// Default channel capacity — large enough to buffer bursts of gate output
/// without back-pressuring the pipeline, small enough to bound memory.
const DEFAULT_CAPACITY: usize = 4096;

/// Central event bus for pipeline observability.
///
/// Clone-friendly via internal `Arc`. All clones share the same underlying
/// broadcast channel — calling `emit()` on any clone delivers to all
/// subscribers created from any clone.
#[derive(Clone)]
pub struct EventBus {
    tx: Arc<broadcast::Sender<PipelineEvent>>,
}

impl EventBus {
    /// Create a new event bus with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create a new event bus with a specific capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx: Arc::new(tx) }
    }

    /// Emit an event to all subscribers.
    ///
    /// If no subscribers exist, the event is silently dropped.
    /// This is intentional — the bus should never block the pipeline.
    pub fn emit(&self, kind: EventKind) {
        let event = PipelineEvent::new(kind);
        // Ignore SendError (means no active receivers — that's fine)
        let _ = self.tx.send(event);
    }

    /// Subscribe to receive pipeline events.
    ///
    /// Each subscriber gets its own independent stream. If a subscriber
    /// falls behind by more than `capacity` events, it will receive a
    /// `RecvError::Lagged` on the next recv — the missed events are lost
    /// for that subscriber but the pipeline is never blocked.
    pub fn subscribe(&self) -> broadcast::Receiver<PipelineEvent> {
        self.tx.subscribe()
    }

    /// Number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::agent::AgentId;
    use thrum_core::event::EventKind;
    use thrum_core::task::{RepoName, TaskId};

    #[tokio::test]
    async fn emit_and_receive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.emit(EventKind::AgentStarted {
            agent_id: AgentId("agent-1".into()),
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
        });

        let event = rx.recv().await.unwrap();
        assert!(matches!(event.kind, EventKind::AgentStarted { .. }));
    }

    #[tokio::test]
    async fn multiple_subscribers_each_get_copy() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.emit(EventKind::EngineLog {
            level: thrum_core::event::LogLevel::Info,
            message: "hello".into(),
        });

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert!(matches!(e1.kind, EventKind::EngineLog { .. }));
        assert!(matches!(e2.kind, EventKind::EngineLog { .. }));
    }

    #[tokio::test]
    async fn emit_without_subscribers_does_not_panic() {
        let bus = EventBus::new();
        // No subscriber — should not panic or block
        bus.emit(EventKind::EngineLog {
            level: thrum_core::event::LogLevel::Info,
            message: "dropped".into(),
        });
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn clone_shares_channel() {
        let bus = EventBus::new();
        let bus2 = bus.clone();
        let mut rx = bus.subscribe();

        // Emit from clone, receive from original's subscriber
        bus2.emit(EventKind::AgentFinished {
            agent_id: AgentId("agent-2".into()),
            task_id: TaskId(42),
            success: true,
            elapsed_secs: 5.0,
        });

        let event = rx.recv().await.unwrap();
        assert!(matches!(
            event.kind,
            EventKind::AgentFinished { success: true, .. }
        ));
    }
}
