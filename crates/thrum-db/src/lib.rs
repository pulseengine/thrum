pub mod budget_store;
pub mod gate_store;
pub mod memory_store;
pub mod task_store;
pub mod trace_store;

use anyhow::Result;
use redb::Database;
use std::path::Path;

/// Open (or create) the automator database at the given path.
pub fn open_db(path: &Path) -> Result<Database> {
    let db = Database::create(path)?;
    // Ensure all tables exist by doing a write transaction
    let write_txn = db.begin_write()?;
    {
        let _tasks = write_txn.open_table(task_store::TASKS_TABLE)?;
        let _counter = write_txn.open_table(task_store::COUNTER_TABLE)?;
        let _gates = write_txn.open_table(gate_store::GATES_TABLE)?;
        let _traces = write_txn.open_table(trace_store::TRACES_TABLE)?;
        let _trace_counter = write_txn.open_table(trace_store::TRACE_COUNTER_TABLE)?;
        let _memory = write_txn.open_table(memory_store::MEMORY_TABLE)?;
        let _budget = write_txn.open_table(budget_store::BUDGET_TABLE)?;
    }
    write_txn.commit()?;
    Ok(db)
}
