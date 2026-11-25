//! Live integration tests for Step 15 (device-delay calibration) and Step 17
//! (opt-in drift clock).

use labstream_net::calibrate::{calibrate_streams, CalibrationOptions};
use labstream_net::{clock, Inlet, Outlet, StreamInfo};
use labstream_time::flags;
use labstream_wire::{Format, PackedSample};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn simulated_device_delay_is_calibrated_within_1_ms_and_applied_opt_in() {
    let srate = 1000.0;
    let true_delay = 0.025; // 25.0 ms fixed hardware delay

    let dev_info = StreamInfo::new("CalibSimDev", "EEG", 2, Format::Float32, srate);
    let ref_info = StreamInfo::new("CalibSimRef", "Markers", 1, Format::String, 0.0);

    let dev_outlet = match Outlet::new(dev_info) {
        Ok(o) => Arc::new(o),
        Err(e) => {
            eprintln!("skipping live calibrate test (no local network): {e}");
            return;
        }
    };
    let ref_outlet = Arc::new(Outlet::new(ref_info).expect("ref outlet"));

    let dev_pub = dev_outlet.info().clone();
    let ref_pub = ref_outlet.info().clone();

    let stop = Arc::new(AtomicBool::new(false));
    let pusher = {
        let dev = Arc::clone(&dev_outlet);
        let ref_o = Arc::clone(&ref_outlet);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            // Wait until both calibration inlets attach.
            if !dev.wait_for_consumers(Duration::from_secs(5))
                || !ref_o.wait_for_consumers(Duration::from_secs(5))
            {
                return;
            }
            // Emit 15 reference pulses every 80 ms, while the simulated device
            // streams at 1000 Hz and raises channel 0 exactly `true_delay` after
            // each reference event timestamp.
            let t_start = clock() + 0.05;
            let pulse_times: Vec<f64> = (0..15).map(|k| t_start + k as f64 * 0.08).collect();
            let mut next_pulse = 0usize;
            let mut sample_idx = 0usize;
            while !stop.load(Ordering::Relaxed) && sample_idx < 1600 {
                let ts = t_start - 0.02 + sample_idx as f64 / srate;
                while next_pulse < pulse_times.len() && ts >= pulse_times[next_pulse] {
                    ref_o.push_packed(PackedSample::from_strings(
                        pulse_times[next_pulse],
                        vec![b"PULSE".to_vec()],
                    ));
                    next_pulse += 1;
                }
                let mut ch0 = 0.0f32;
                for &tp in &pulse_times {
                    let edge = tp + true_delay;
                    if ts >= edge && ts < edge + 0.015 {
                        ch0 = 1.0;
                        break;
                    }
                }
                dev.push_packed(PackedSample::from_f32(ts, &[ch0, 0.0]));
                sample_idx += 1;
                if sample_idx % 10 == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        })
    };

    let opts = CalibrationOptions {
        channel: 0,
        target_pulses: 12,
        timeout: Duration::from_secs(10),
        ..CalibrationOptions::default()
    };
    let result = calibrate_streams(&dev_pub, &ref_pub, &opts).expect("calibration succeeds");
    stop.store(true, Ordering::Relaxed);
    let _ = pusher.join();

    // Criterion 1: error < 1 ms against the known 25.0 ms simulated delay.
    assert!(
        result.matched_pulses >= 10,
        "expected >= 10 matched pulses, got {}",
        result.matched_pulses
    );
    let err_ms = (result.delay_mean_s - true_delay).abs() * 1000.0;
    assert!(
        err_ms < 1.0,
        "expected calibration error < 1 ms, got {err_ms:.3} ms (measured {:.3} ms)",
        result.delay_mean_s * 1000.0
    );

    // Criterion 2: store in `<desc><synchronization><offset_mean>` and confirm
    // full XML (XDF StreamHeader) round-trip.
    let mut cal_info = StreamInfo::new("CalibratedEEG", "EEG", 1, Format::Float32, 500.0);
    result.apply_to_info(&mut cal_info);
    let cal_outlet = Outlet::new(cal_info).expect("calibrated outlet");
    let xml = cal_outlet.info().to_fullinfo_xml();
    assert!(xml.contains("<synchronization>"), "xml={xml}");
    assert!(xml.contains("<offset_mean>"), "xml={xml}");
    let reparsed = StreamInfo::from_fullinfo_xml(&xml).expect("parse fullinfo");
    assert!(
        (reparsed.device_delay().unwrap() - result.delay_mean_s).abs() < 1e-5,
        "XDF XML round-trip mismatch"
    );

    // Criterion 3: opt-in correction leaves default timestamps unchanged, and
    // subtracts the calibrated delay only when `flags::DEVICE_DELAY` is set.
    let mut default_inlet =
        Inlet::open(cal_outlet.info(), Duration::from_secs(5)).expect("default inlet");
    let mut corrected_inlet =
        Inlet::open(cal_outlet.info(), Duration::from_secs(5)).expect("corrected inlet");
    let wait_end = std::time::Instant::now() + Duration::from_secs(5);
    while cal_outlet.consumer_count() < 2 && std::time::Instant::now() < wait_end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(cal_outlet.consumer_count(), 2);
    corrected_inlet.set_postprocessing(flags::DEVICE_DELAY);

    let pushed_ts = 5000.025;
    cal_outlet.push_packed(PackedSample::from_f32(pushed_ts, &[42.0]));
    let s_def = default_inlet
        .pull_packed(Duration::from_secs(2))
        .unwrap()
        .expect("default sample");
    let s_cor = corrected_inlet
        .pull_packed(Duration::from_secs(2))
        .unwrap()
        .expect("corrected sample");

    assert_eq!(
        s_def.timestamp.to_bits(),
        pushed_ts.to_bits(),
        "default timestamps must not change"
    );
    assert!(
        ((pushed_ts - s_cor.timestamp) - result.delay_mean_s).abs() < 1e-5,
        "DEVICE_DELAY must subtract calibrated delay"
    );
}

#[test]
fn opt_in_drift_clock_runs_smoothly_and_reports_health() {
    let info = StreamInfo::new("DriftLive0", "EEG", 1, Format::Float32, 200.0);
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live drift test (no local network): {e}");
            return;
        }
    };
    let mut inlet = Inlet::open(outlet.info(), Duration::from_secs(5)).expect("open inlet");
    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));
    inlet.set_postprocessing(flags::CLOCKSYNC | flags::DRIFT | flags::MONOTONIZE);

    // Wait for initial time-sync burst.
    assert!(inlet
        .wait_for_time_correction(Duration::from_secs(3))
        .is_some());
    assert!(inlet.clock_drift().is_some());
    let h = inlet.health();
    assert!(h.clock_drift.is_some());

    let t0 = clock();
    for k in 0..20 {
        outlet.push_packed(PackedSample::from_f32(t0 + k as f64 * 0.005, &[k as f32]));
    }
    let mut prev = f64::NEG_INFINITY;
    for _ in 0..20 {
        let s = inlet
            .pull_packed(Duration::from_secs(2))
            .unwrap()
            .expect("sample");
        assert!(s.timestamp >= prev);
        // On loopback, local and remote share CLOCK_MONOTONIC, so smooth
        // corrected timestamps stay within 2 ms of pushed local time.
        assert!((s.timestamp - t0).abs() < 0.2);
        prev = s.timestamp;
    }
}
