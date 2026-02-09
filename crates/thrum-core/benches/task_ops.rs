use criterion::{Criterion, black_box, criterion_group, criterion_main};
use thrum_core::task::{RepoName, Task, TaskStatus};

fn bench_task_creation(c: &mut Criterion) {
    c.bench_function("task_new", |b| {
        b.iter(|| {
            Task::new(
                black_box(RepoName::new("loom")),
                black_box("Implement i32.popcnt in ISLE pipeline".into()),
                black_box("Add the popcount intrinsic lowering rule".into()),
            )
        })
    });
}

fn bench_branch_name(c: &mut Criterion) {
    let mut task = Task::new(
        RepoName::new("synth"),
        "Add ARM Cortex-M4 backend with SIMD support".into(),
        "d".into(),
    );
    task.id = thrum_core::task::TaskId(42);

    c.bench_function("branch_name", |b| b.iter(|| black_box(&task).branch_name()));
}

fn bench_status_label(c: &mut Criterion) {
    let statuses = vec![
        TaskStatus::Pending,
        TaskStatus::Claimed {
            agent_id: "agent-1".into(),
            claimed_at: chrono::Utc::now(),
        },
        TaskStatus::Implementing {
            branch: "auto/TASK-0001/loom/test".into(),
            started_at: chrono::Utc::now(),
        },
        TaskStatus::Approved,
    ];

    c.bench_function("status_label_all", |b| {
        b.iter(|| {
            for s in &statuses {
                black_box(s.label());
            }
        })
    });
}

fn bench_task_serde_roundtrip(c: &mut Criterion) {
    let task = Task::new(
        RepoName::new("meld"),
        "Fuse WASM modules with dead code elimination".into(),
        "Apply tree-shaking pass before module linking".into(),
    );

    c.bench_function("task_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&task)).unwrap())
    });

    let json = serde_json::to_string(&task).unwrap();
    c.bench_function("task_deserialize", |b| {
        b.iter(|| serde_json::from_str::<Task>(black_box(&json)).unwrap())
    });
}

fn bench_repo_name_parse(c: &mut Criterion) {
    c.bench_function("repo_name_parse", |b| {
        b.iter(|| black_box("My-Custom-Project").parse::<RepoName>().unwrap())
    });
}

fn bench_memory_content_hash(c: &mut Criterion) {
    use thrum_core::memory::{MemoryCategory, MemoryEntry};
    use thrum_core::task::TaskId;

    let content = "Failed to compile: missing trait implementation for `Into<WasmValue>` on type `i64`. The ARM backend needs explicit conversion.".to_string();

    c.bench_function("memory_entry_new", |b| {
        b.iter(|| {
            MemoryEntry::new(
                TaskId(1),
                RepoName::new("loom"),
                MemoryCategory::Error {
                    error_type: "compile".into(),
                },
                black_box(content.clone()),
            )
        })
    });
}

criterion_group!(
    benches,
    bench_task_creation,
    bench_branch_name,
    bench_status_label,
    bench_task_serde_roundtrip,
    bench_repo_name_parse,
    bench_memory_content_hash,
);
criterion_main!(benches);
