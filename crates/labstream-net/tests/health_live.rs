//! Live tests for the Step 11 Health API (`docs/health.md`).

use labstream_net::{clock, GapKind, Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use std::time::{Duration, Instant};

fn sample_at(ts: f64, v: f32) -> Sample {
    Sample {
        timestamp: ts,
        values: vec![Value::F32(v)],
    }
}

#[test]
fn inlet_health_reports_latency_uncertainty_and_buffer_state() {
    let name = format!("HealthBasic_{}", std::process::id());
    let info = StreamInfo::new(&name, "EEG", 1, Format::Float32, 250.0);
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live test (no local network): {e}");
            return;
        }
    };
    let mut inlet = Inlet::open_with_buffer(outlet.info(), Duration::from_secs(5), 256).unwrap();
    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));

    // Wait for clock sync so both `clock_uncertainty` and `latency` are available.
    assert!(inlet
        .wait_for_time_correction(Duration::from_secs(5))
        .is_some());

    // Push live timestamped samples and confirm arrival latency is measured.
    for k in 0..10 {
        outlet.push(&sample_at(clock(), k as f32));
        std::thread::sleep(Duration::from_millis(2));
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && inlet.samples_available() < 10 {
        std::thread::sleep(Duration::from_millis(5));
    }

    let h = inlet.health();
    assert!(h.connected);
    assert_eq!(h.lost, Some(0));
    assert_eq!(h.resent, Some(0));
    assert_eq!(h.dropped, 0);
    assert_eq!(h.recoveries, 0);
    assert_eq!(h.buffer_capacity, 256);
    assert_eq!(h.buffered, 10);
    assert_eq!(h.gap_count, 0);
    let unc = h.clock_uncertainty.expect("clock uncertainty measured");
    assert!((0.0..1.0).contains(&unc), "unexpected uncertainty: {unc}");
    let lat = h.latency.expect("arrival latency measured");
    assert!((0.0..1.0).contains(&lat), "unexpected latency: {lat}");
    assert!(inlet.gaps().is_empty());

    // Pulling drains the buffer and reflects in `health().buffered`.
    let mut pulled = Vec::new();
    let got = inlet
        .pull_chunk(&mut pulled, 32, Duration::from_secs(2))
        .unwrap();
    assert_eq!(got, 10);
    assert_eq!(inlet.health().buffered, 0);
}

#[test]
fn inlet_reports_drop_gap_and_outlet_reports_per_consumer_health() {
    let name = format!("HealthMulti_{}", std::process::id());
    let info = StreamInfo::new(&name, "EEG", 1, Format::Float32, 200.0);
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live test (no local network): {e}");
            return;
        }
    };
    let mut fast = Inlet::open_with_buffer(outlet.info(), Duration::from_secs(5), 512).unwrap();
    let mut slow = Inlet::open_with_buffer(outlet.info(), Duration::from_secs(5), 4).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && outlet.consumer_count() < 2 {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(outlet.consumer_count(), 2);

    // Pull one initial sample on both inlets at ts = 500.0.
    outlet.push(&sample_at(500.0, 0.0));
    assert_eq!(
        fast.pull(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .timestamp,
        500.0
    );
    assert_eq!(
        slow.pull(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .timestamp,
        500.0
    );

    // Push 30 samples with brief pacing so the outlet drains cleanly to `slow`
    // and `slow`'s 4-sample inlet buffer overflows rather than its outlet queue.
    for k in 1..=30 {
        outlet.push(&sample_at(500.0 + k as f64, k as f32));
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut fast_got = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && (fast_got < 30 || slow.samples_dropped() < 26) {
        let mut tmp = Vec::new();
        fast_got += fast
            .pull_chunk(&mut tmp, 64, Duration::from_millis(10))
            .unwrap();
    }
    assert_eq!(fast_got, 30);
    assert_eq!(fast.health().dropped, 0);
    assert!(fast.gaps().is_empty());

    let slow_health = slow.health();
    assert!(
        slow_health.dropped >= 26,
        "expected slow inlet drops, got {slow_health:?}"
    );
    assert!(slow_health.gap_count >= 1);

    let next_slow = slow.pull(Duration::from_secs(2)).unwrap().unwrap();
    let gaps = slow.take_gaps();
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].kind, GapKind::Dropped);
    assert_eq!(gaps[0].start, 500.0);
    assert_eq!(gaps[0].end, next_slow.timestamp);
    assert_eq!(gaps[0].count, slow_health.dropped);
    assert!(slow.gaps().is_empty());

    // Outlet health reports both consumers, their capacities, peers, and pushed count.
    let oh = outlet.health();
    assert_eq!(oh.pushed, 31);
    assert_eq!(oh.consumers.len(), 2);
    assert_ne!(oh.consumers[0].id, oh.consumers[1].id);
    for c in &oh.consumers {
        assert!(!c.peer.is_empty(), "expected non-empty peer address");
        assert!(c.capacity == 512 || c.capacity == 4);
    }
}

#[test]
fn inlet_health_counts_recovery_after_outlet_restart() {
    let name = format!("HealthRecover_{}", std::process::id());
    let src = format!("health-recover-src-{}", std::process::id());
    let mut info = StreamInfo::new(&name, "EEG", 1, Format::Float32, 100.0);
    info.source_id = src.clone();

    let outlet1 = match Outlet::new(info.clone()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live test (no local network): {e}");
            return;
        }
    };
    let mut inlet = Inlet::open_recovering(outlet1.info(), Duration::from_secs(5), 256).unwrap();
    assert!(outlet1.wait_for_consumers(Duration::from_secs(5)));
    outlet1.push(&sample_at(10.0, 1.0));
    assert!(inlet.pull(Duration::from_secs(2)).unwrap().is_some());
    assert_eq!(inlet.health().recoveries, 0);

    // Restart the outlet with the same source_id; inlet must recover and count it.
    drop(outlet1);
    std::thread::sleep(Duration::from_millis(100));
    let outlet2 = Outlet::new(info).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && inlet.health().recoveries < 1 {
        std::thread::sleep(Duration::from_millis(50));
    }
    let h = inlet.health();
    assert!(h.recoveries >= 1, "expected recovery, got {h:?}");
    assert!(h.connected);
    outlet2.push(&sample_at(20.0, 2.0));
    assert!(inlet.pull(Duration::from_secs(2)).unwrap().is_some());
}

#[test]
fn inlet_and_outlet_health_report_lost_gap_when_outlet_queue_overflows() {
    let name = format!("HealthLost_{}", std::process::id());
    let info = StreamInfo::new(&name, "EEG", 1, Format::Float32, 1000.0);
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live test (no local network): {e}");
            return;
        }
    };
    let mut inlet = Inlet::open_recovering(outlet.info(), Duration::from_secs(5), 32).unwrap();
    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));

    // Establish baseline sample at ts = 1000.0.
    outlet.push(&sample_at(1000.0, 0.0));
    let first = inlet.pull(Duration::from_secs(5)).unwrap().unwrap();
    assert_eq!(first.timestamp, 1000.0);

    // Push 100_000 samples in one burst so the 32-sample outlet queue drops most
    // of the block and exceeds the 30 s (30_000-sample) resume history.
    let n = 100_000usize;
    let block: Vec<Sample> = (1..=n)
        .map(|k| sample_at(1000.0 + k as f64 * 0.001, k as f32))
        .collect();
    outlet.push_samples(block);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_last = false;
    while Instant::now() < deadline && !saw_last {
        let mut chunk = Vec::new();
        let _ = inlet.pull_chunk(&mut chunk, 100_000, Duration::from_millis(20));
        if chunk
            .iter()
            .any(|s| matches!(s.values[0], Value::F32(v) if v as usize == n))
        {
            saw_last = true;
        }
    }
    assert!(saw_last, "expected last sample {n} to arrive");

    let oh = outlet.health();
    assert_eq!(oh.consumers.len(), 1);
    assert!(
        oh.consumers[0].dropped > 0,
        "expected outlet consumer queue drops, got {oh:?}"
    );

    let ih = inlet.health();
    assert!(
        ih.lost.unwrap_or(0) > 0,
        "expected seq/dgram lost count > 0, got {ih:?}"
    );
    let gaps = inlet.gaps();
    let lost_gaps: Vec<_> = gaps.iter().filter(|g| g.kind == GapKind::Lost).collect();
    assert!(
        !lost_gaps.is_empty(),
        "expected at least one GapKind::Lost entry, got {gaps:?}"
    );
    for g in &lost_gaps {
        assert!(g.start >= 1000.0, "unexpected start: {g:?}");
        assert!(g.end > g.start, "expected end > start: {g:?}");
        assert!(g.count > 0, "expected positive gap count: {g:?}");
    }
}
