use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use keywatch::{Update, channel};
use std::{hint::black_box, time::Duration};
use tokio::runtime::Runtime;

fn bench_single_key_throughput(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Tune criterion to reduce long sample times (no apparent deadlocks, original loop just too long).
    let mut group = c.benchmark_group("keywatch");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(30); // keep samples bounded

    group.bench_function("single_key_adds_no_cooldown", |b| {
        b.to_async(&rt).iter(|| async {
            let (tx, mut rx) = channel::<u32, u32>(Duration::ZERO);
            for i in 0..1_000u32 {
                // smaller inner loop for fast samples
                tx.send(0, Update::Add(i)).unwrap();
                let v = rx.recv().await.unwrap();
                black_box(v);
            }
        })
    });

    group.bench_function("single_key_adds_with_cooldown_delivered", |b| {
        b.to_async(&rt).iter(|| async {
            // Sleep >= cooldown so each Add is eventually emitted; fewer iterations to bound time.
            let cooldown = Duration::from_millis(2);
            let (tx, mut rx) = channel::<u32, u32>(cooldown);
            for i in 0..200u32 {
                tx.send(0, Update::Add(i)).unwrap();
                // receive prior (if any)
                let _ = rx.try_recv();
                tokio::time::sleep(cooldown).await; // allow maturation
                if let Ok(ev) = rx.try_recv() {
                    black_box(ev);
                }
            }
        })
    });

    group.bench_function("single_key_high_update_pressure_cooldown_merge", |b| {
        b.to_async(&rt).iter(|| async {
            // Send faster than cooldown; expect heavy coalescing; measure send overhead.
            let cooldown = Duration::from_millis(5);
            let (tx, mut rx) = channel::<u32, u32>(cooldown);
            for i in 0..1_000u32 {
                tx.send(0, Update::Add(i)).unwrap();
                // Occasionally drain if something matured (rare)
                if i % 50 == 0 {
                    let _ = rx.try_recv();
                }
                tokio::time::sleep(Duration::from_micros(200)).await; // << cooldown
            }
            // Final maturation window
            tokio::time::sleep(cooldown).await;
            while let Ok(ev) = rx.try_recv() {
                black_box(ev);
            }
        })
    });

    group.bench_function("multi_key_round_robin", |b| {
        b.to_async(&rt).iter(|| async {
            let (tx, mut rx) = channel::<u32, u32>(Duration::ZERO);
            for k in 0..200u32 {
                tx.send(k, Update::Add(k)).unwrap();
            }
            for _ in 0..200u32 {
                if let Some(ev) = rx.recv().await {
                    black_box(ev);
                }
            }
        })
    });

    group.finish();
}

criterion_group!(benches, bench_single_key_throughput);
criterion_main!(benches);
