//! Live integration test for step 18 (real-time multi-stream sync and resampler).

use labstream_net::{Inlet, Outlet, StreamInfo, SyncConfig, SyncGroup};
use labstream_wire::Format;
use std::time::{Duration, Instant};

#[test]
fn live_sync_group_aligns_two_async_250sps_100hz_outlets_below_2_deg() {
    let fs = 250.0f64;
    let f_sig = 100.0f64;
    let dt = 1.0 / fs;
    let half_sample = 0.5 * dt; // 2 ms worst-case offset -> 72 deg at 100 Hz

    let mut info0 = StreamInfo::new("SyncAdcA", "EEG", 1, Format::Float32, fs);
    info0.source_id = format!("sync-a-{}", std::process::id());
    let mut info1 = StreamInfo::new("SyncAdcB", "EEG", 1, Format::Float32, fs);
    info1.source_id = format!("sync-b-{}", std::process::id());

    let out0 = Outlet::new(info0).expect("out0");
    let out1 = Outlet::new(info1).expect("out1");

    let in0 = Inlet::open_with_buffer(out0.info(), Duration::from_secs(2), 4096).expect("in0");
    let in1 = Inlet::open_with_buffer(out1.info(), Duration::from_secs(2), 4096).expect("in1");
    assert!(out0.wait_for_consumers(Duration::from_secs(2)));
    assert!(out1.wait_for_consumers(Duration::from_secs(2)));

    // Use raw synthetic timestamps so the exact 2 ms sub-sample phase offset is preserved.
    let mut cfg = SyncConfig::new(fs);
    cfg.auto_postprocess = false;
    let mut group = SyncGroup::new(vec![in0, in1], cfg).expect("SyncGroup");
    assert_eq!(group.total_channels(), 2);
    assert_eq!(group.stream_count(), 2);

    // Push 300 samples on both outlets with a 2 ms (half-sample) phase offset.
    let n = 300usize;
    let mut v0 = Vec::with_capacity(n);
    let mut v1 = Vec::with_capacity(n);
    let t0_start = 1000.0f64;
    for k in 0..n {
        let t0 = t0_start + k as f64 * dt;
        let t1 = t0_start + k as f64 * dt + half_sample;
        v0.push((2.0 * core::f64::consts::PI * f_sig * t0).sin() as f32);
        v1.push((2.0 * core::f64::consts::PI * f_sig * t1).sin() as f32);
    }
    out0.push_chunk_f32(&v0, t0_start + (n - 1) as f64 * dt);
    out1.push_chunk_f32(&v1, t0_start + (n - 1) as f64 * dt + half_sample);

    let mut aligned = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while aligned.len() < 200 && Instant::now() < deadline {
        let _ = group
            .pull(&mut aligned, 256, Duration::from_millis(50))
            .unwrap();
    }
    assert!(
        aligned.len() >= 200,
        "got only {} aligned samples",
        aligned.len()
    );

    let slice = &aligned[10..190];
    let mut sum_sq_err = 0.0f64;
    let mut sum_sq_ref = 0.0f64;
    for (i, s) in slice.iter().enumerate() {
        assert!(!s.any_gap());
        if i > 0 {
            assert!(s.timestamp > slice[i - 1].timestamp);
        }
        let d = s.values[0] - s.values[1];
        sum_sq_err += d * d;
        sum_sq_ref += s.values[0] * s.values[0];
    }
    let rel_rms = (sum_sq_err / sum_sq_ref).sqrt();
    let phase_err_deg = 2.0 * (rel_rms / 2.0).min(1.0).asin() * 180.0 / core::f64::consts::PI;
    assert!(
        phase_err_deg < 2.0,
        "live SyncGroup phase error {phase_err_deg:.4} deg must be < 2 deg (vs 72 deg nearest-sample)"
    );
}
