use criterion::{black_box, criterion_group, criterion_main, Criterion};
use idempotency_rs::{IdempotencyMiddleware, SqliteIdempotencyStore};
use uuid::Uuid;

async fn benchmark_process_request() {
    let store = SqliteIdempotencyStore::new(":memory:").await.unwrap();
    let middleware = IdempotencyMiddleware::new(store);

    let key = Uuid::new_v4().to_string();
    let request = serde_json::json!({
        "test": "data",
        "number": 42
    });

    let _ = middleware.process_request(
        Some(key),
        "user123".to_string(),
        "/test".to_string(),
        "POST".to_string(),
        &request,
        || async { Ok((200, Default::default(), "response")) },
    ).await;
}

fn criterion_benchmark(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("process_new_request", |b| {
        b.to_async(&runtime).iter(|| benchmark_process_request());
    });

    // Add more benchmarks...
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
