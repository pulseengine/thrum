use criterion::{Criterion, black_box, criterion_group, criterion_main};
use thrum_core::task::{RepoName, Task, TaskId};
use thrum_db::task_store::{ClaimCategory, TaskStore};

fn open_temp_db() -> (tempfile::TempDir, redb::Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = thrum_db::open_db(&dir.path().join("bench.redb")).unwrap();
    (dir, db)
}

fn bench_insert(c: &mut Criterion) {
    let (_dir, db) = open_temp_db();
    let store = TaskStore::new(&db);

    c.bench_function("store_insert", |b| {
        b.iter(|| {
            store
                .insert(Task::new(
                    RepoName::new("loom"),
                    black_box("Benchmark task".into()),
                    black_box("d".into()),
                ))
                .unwrap()
        })
    });
}

fn bench_get(c: &mut Criterion) {
    let (_dir, db) = open_temp_db();
    let store = TaskStore::new(&db);

    // Pre-populate
    for i in 0..100 {
        store
            .insert(Task::new(
                RepoName::new("loom"),
                format!("Task {i}"),
                "d".into(),
            ))
            .unwrap();
    }

    c.bench_function("store_get", |b| {
        b.iter(|| store.get(black_box(&TaskId(50))).unwrap())
    });
}

fn bench_list_all(c: &mut Criterion) {
    let (_dir, db) = open_temp_db();
    let store = TaskStore::new(&db);

    for i in 0..100 {
        let repo = if i % 3 == 0 {
            "loom"
        } else if i % 3 == 1 {
            "synth"
        } else {
            "meld"
        };
        store
            .insert(Task::new(
                RepoName::new(repo),
                format!("Task {i}"),
                "d".into(),
            ))
            .unwrap();
    }

    c.bench_function("store_list_100", |b| {
        b.iter(|| store.list(None, None).unwrap())
    });

    c.bench_function("store_list_filtered_repo", |b| {
        let repo = RepoName::new("loom");
        b.iter(|| store.list(None, Some(black_box(&repo))).unwrap())
    });

    c.bench_function("store_list_filtered_status", |b| {
        b.iter(|| store.list(Some(black_box("pending")), None).unwrap())
    });
}

fn bench_claim_next(c: &mut Criterion) {
    let (_dir, db) = open_temp_db();
    let store = TaskStore::new(&db);

    // Pre-populate 50 tasks
    for i in 0..50 {
        store
            .insert(Task::new(
                RepoName::new("loom"),
                format!("Task {i}"),
                "d".into(),
            ))
            .unwrap();
    }

    let mut agent_counter = 0u64;
    c.bench_function("store_claim_next", |b| {
        b.iter(|| {
            agent_counter += 1;
            store.claim_next(
                black_box(&format!("agent-{agent_counter}")),
                ClaimCategory::Pending,
                None,
            )
        })
    });
}

fn bench_status_counts(c: &mut Criterion) {
    let (_dir, db) = open_temp_db();
    let store = TaskStore::new(&db);

    for i in 0..100 {
        store
            .insert(Task::new(
                RepoName::new("loom"),
                format!("Task {i}"),
                "d".into(),
            ))
            .unwrap();
    }
    // Claim some to diversify statuses
    for i in 0..20 {
        store
            .claim_next(&format!("agent-{i}"), ClaimCategory::Pending, None)
            .unwrap();
    }

    c.bench_function("store_status_counts_100", |b| {
        b.iter(|| store.status_counts().unwrap())
    });
}

criterion_group!(
    benches,
    bench_insert,
    bench_get,
    bench_list_all,
    bench_claim_next,
    bench_status_counts,
);
criterion_main!(benches);
