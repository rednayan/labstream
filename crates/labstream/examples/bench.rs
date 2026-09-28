//! Measure the block calls against the one-sample calls.
//!
//! Each variant runs alone: the queue is built, used, and dropped before the
//! next variant starts. Each is measured five times and the fastest is kept.
//! The order alternates, so a warm allocator cannot favour one of them.
use labstream_net::queue::SampleQueue;
use labstream_wire::{Sample, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn block(n: usize, ch: usize) -> Vec<Sample> {
    (0..n)
        .map(|k| Sample {
            timestamp: k as f64,
            values: (0..ch).map(|c| Value::F32((k * ch + c) as f32)).collect(),
        })
        .collect()
}

fn best(runs: usize, mut f: impl FnMut() -> Duration) -> Duration {
    (0..runs).map(|_| f()).min().unwrap()
}

fn main() {
    const N: usize = 100_000;
    const BLK: usize = 500;
    for &ch in &[8usize, 64] {
        let data = block(N, ch);
        println!("\n{ch} channels, {N} samples, blocks of {BLK}");

        // ── uncontended write ────────────────────────────────────────────────
        let w1 = best(5, || {
            let q = SampleQueue::new(N);
            let t = Instant::now();
            for s in &data {
                q.push(s.clone());
            }
            t.elapsed()
        });
        let w2 = best(5, || {
            let q = SampleQueue::new(N);
            let t = Instant::now();
            for c in data.chunks(BLK) {
                q.push_many(c);
            }
            t.elapsed()
        });
        println!(
            "  write   one {w1:>9.2?}   block {w2:>9.2?}   {:.2}x",
            w1.as_secs_f64() / w2.as_secs_f64()
        );

        // ── uncontended read ─────────────────────────────────────────────────
        let r1 = best(5, || {
            let q = SampleQueue::new(N);
            for s in &data {
                q.push(s.clone());
            }
            let t = Instant::now();
            while q.pop(Duration::ZERO).is_some() {}
            t.elapsed()
        });
        let r2 = best(5, || {
            let q = SampleQueue::new(N);
            for s in &data {
                q.push(s.clone());
            }
            let mut out = Vec::with_capacity(BLK);
            let t = Instant::now();
            loop {
                out.clear();
                if q.pop_many(&mut out, BLK, Duration::ZERO) == 0 {
                    break;
                }
            }
            t.elapsed()
        });
        println!(
            "  read    one {r1:>9.2?}   block {r2:>9.2?}   {:.2}x",
            r1.as_secs_f64() / r2.as_secs_f64()
        );

        // ── the real case: a reader thread writes while the app reads ────────
        let c1 = best(3, || contended(&data, N, false));
        let c2 = best(3, || contended(&data, N, true));
        println!(
            "  live    one {c1:>9.2?}   block {c2:>9.2?}   {:.2}x",
            c1.as_secs_f64() / c2.as_secs_f64()
        );
    }
}

/// One thread writes every sample while another reads them, as an inlet does.
fn contended(data: &[Sample], n: usize, batch: bool) -> Duration {
    let q = Arc::new(SampleQueue::new(n));
    let w = Arc::clone(&q);
    let owned: Vec<Sample> = data.to_vec();
    let t = Instant::now();
    let writer = std::thread::spawn(move || {
        for s in &owned {
            w.push(s.clone());
        }
    });
    let mut got = 0usize;
    let mut out = Vec::with_capacity(500);
    while got < n {
        if batch {
            out.clear();
            got += q.pop_many(&mut out, 500, Duration::from_millis(1));
        } else if q.pop(Duration::from_millis(1)).is_some() {
            got += 1;
        }
    }
    writer.join().unwrap();
    t.elapsed()
}
