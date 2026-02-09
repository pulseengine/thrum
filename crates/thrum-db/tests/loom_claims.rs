//! Loom model-check of the concurrent claim protocol.
//!
//! Since redb uses real file I/O that loom can't intercept, we model
//! the claim logic with loom primitives to verify no double-dispatch.

#[cfg(test)]
mod tests {
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    /// Model of the atomic claim protocol used by TaskStore::claim_next.
    /// A single-writer transaction ensures at most one thread claims each task.
    struct ClaimModel {
        /// Task slots: None = unclaimed, Some(thread_id) = claimed by thread
        tasks: Mutex<Vec<Option<usize>>>,
    }

    impl ClaimModel {
        fn new(num_tasks: usize) -> Self {
            Self {
                tasks: Mutex::new(vec![None; num_tasks]),
            }
        }

        /// Attempt to claim the first unclaimed task. Returns task index if claimed.
        fn claim_next(&self, thread_id: usize) -> Option<usize> {
            let mut tasks = self.tasks.lock().unwrap();
            for (i, slot) in tasks.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(thread_id);
                    return Some(i);
                }
            }
            None
        }
    }

    #[test]
    fn no_double_claim_two_threads_one_task() {
        loom::model(|| {
            let model = Arc::new(ClaimModel::new(1));
            let m1 = Arc::clone(&model);
            let m2 = Arc::clone(&model);

            let t1 = thread::spawn(move || m1.claim_next(1));
            let t2 = thread::spawn(move || m2.claim_next(2));

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Exactly one thread should claim the task
            assert!(
                r1.is_some() ^ r2.is_some(),
                "exactly one thread should claim the task"
            );
        });
    }

    #[test]
    fn no_double_claim_two_threads_two_tasks() {
        loom::model(|| {
            let model = Arc::new(ClaimModel::new(2));
            let m1 = Arc::clone(&model);
            let m2 = Arc::clone(&model);

            let t1 = thread::spawn(move || m1.claim_next(1));
            let t2 = thread::spawn(move || m2.claim_next(2));

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Both should claim, but different tasks
            assert!(r1.is_some());
            assert!(r2.is_some());
            assert_ne!(r1, r2, "threads should claim different tasks");
        });
    }
}
