use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use thrum_core::traceability::TraceRecord;

/// Trace records table: i64 trace ID -> JSON-serialized TraceRecord.
pub const TRACES_TABLE: TableDefinition<i64, &str> = TableDefinition::new("traces");

/// Counter for trace IDs.
pub const TRACE_COUNTER_TABLE: TableDefinition<&str, i64> = TableDefinition::new("trace_counters");

const NEXT_TRACE_ID: &str = "next_trace_id";

pub struct TraceStore<'a> {
    db: &'a Database,
}

impl<'a> TraceStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Insert a trace record.
    pub fn insert(&self, mut record: TraceRecord) -> Result<TraceRecord> {
        let write_txn = self.db.begin_write()?;
        {
            let mut counter = write_txn.open_table(TRACE_COUNTER_TABLE)?;
            let next_id = counter.get(NEXT_TRACE_ID)?.map(|v| v.value()).unwrap_or(1);
            record.id = next_id;
            counter.insert(NEXT_TRACE_ID, next_id + 1)?;

            let json = serde_json::to_string(&record)?;
            let mut traces = write_txn.open_table(TRACES_TABLE)?;
            traces.insert(next_id, json.as_str())?;
        }
        write_txn.commit()?;
        Ok(record)
    }

    /// Get all trace records for a task.
    pub fn get_for_task(&self, task_id: i64) -> Result<Vec<TraceRecord>> {
        let read_txn = self.db.begin_read()?;
        let traces = read_txn.open_table(TRACES_TABLE)?;
        let mut result = Vec::new();

        let iter = traces.iter()?;
        for entry in iter {
            let (_, value) = entry?;
            let record: TraceRecord = serde_json::from_str(value.value())?;
            if record.task_id == task_id {
                result.push(record);
            }
        }

        Ok(result)
    }

    /// Get all trace records for a requirement ID.
    pub fn get_for_requirement(&self, requirement_id: &str) -> Result<Vec<TraceRecord>> {
        let read_txn = self.db.begin_read()?;
        let traces = read_txn.open_table(TRACES_TABLE)?;
        let mut result = Vec::new();

        let iter = traces.iter()?;
        for entry in iter {
            let (_, value) = entry?;
            let record: TraceRecord = serde_json::from_str(value.value())?;
            if record.requirement_id == requirement_id {
                result.push(record);
            }
        }

        Ok(result)
    }

    /// Get a trace record by ID.
    pub fn get(&self, id: i64) -> Result<Option<TraceRecord>> {
        let read_txn = self.db.begin_read()?;
        let traces = read_txn.open_table(TRACES_TABLE)?;

        match traces.get(id)? {
            Some(guard) => {
                let record: TraceRecord = serde_json::from_str(guard.value())
                    .context("failed to deserialize trace record")?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }
}
