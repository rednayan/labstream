//! Live tests for step 8: flat typed API and shared samples (`docs/roadmap.md`).

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_wire::Format;
use std::time::Duration;

#[test]
fn flat_typed_push_and_pull_round_trip_all_numeric_formats_bit_exact() {
    let rate = 200.0;
    let channels = 4usize;

    // Float32
    {
        let mut info = StreamInfo::new("FlatF32", "EEG", channels as u32, Format::Float32, rate);
        info.source_id = "flat-f32".to_string();
        let outlet = Outlet::new(info).unwrap();
        let mut inlet =
            Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 1024).unwrap();
        assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

        let want: Vec<f32> = (0..16).map(|i| (i as f32) * 0.25 - 1.5).collect();
        outlet.push_chunk_f32(&want, 100.0);

        let mut got = Vec::new();
        let mut ts = Vec::new();
        while ts.len() < 4 {
            let rem = 4 - ts.len();
            let n = inlet
                .pull_chunk_f32(&mut got, &mut ts, rem, Duration::from_secs(2))
                .unwrap();
            assert!(n > 0);
        }
        assert_eq!(got.len(), want.len());
        for (a, b) in got.iter().zip(&want) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
        assert!((ts[3] - 100.0).abs() < 1e-9);
    }

    // Double64
    {
        let mut info = StreamInfo::new("FlatF64", "EEG", channels as u32, Format::Double64, rate);
        info.source_id = "flat-f64".to_string();
        let outlet = Outlet::new(info).unwrap();
        let mut inlet =
            Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 1024).unwrap();
        assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

        let want: Vec<f64> = (0..16).map(|i| (i as f64) * 1e-6 + 42.125).collect();
        outlet.push_chunk_f64(&want, 200.0);

        let mut got = Vec::new();
        let mut ts = Vec::new();
        while ts.len() < 4 {
            let rem = 4 - ts.len();
            let n = inlet
                .pull_chunk_f64(&mut got, &mut ts, rem, Duration::from_secs(2))
                .unwrap();
            assert!(n > 0);
        }
        for (a, b) in got.iter().zip(&want) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    // Int32
    {
        let mut info = StreamInfo::new("FlatI32", "EEG", channels as u32, Format::Int32, rate);
        info.source_id = "flat-i32".to_string();
        let outlet = Outlet::new(info).unwrap();
        let mut inlet =
            Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 1024).unwrap();
        assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

        let want: Vec<i32> = (0..16).map(|i| i * 100_000 - 500_000).collect();
        outlet.push_chunk_i32(&want, 300.0);

        let mut got = Vec::new();
        let mut ts = Vec::new();
        while ts.len() < 4 {
            let rem = 4 - ts.len();
            let n = inlet
                .pull_chunk_i32(&mut got, &mut ts, rem, Duration::from_secs(2))
                .unwrap();
            assert!(n > 0);
        }
        assert_eq!(got, want);
    }

    // Int16
    {
        let mut info = StreamInfo::new("FlatI16", "EEG", channels as u32, Format::Int16, rate);
        info.source_id = "flat-i16".to_string();
        let outlet = Outlet::new(info).unwrap();
        let mut inlet =
            Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 1024).unwrap();
        assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

        let want: Vec<i16> = (0..16).map(|i| i * 1000 - 8000).collect();
        outlet.push_chunk_i16(&want, 400.0);

        let mut got = Vec::new();
        let mut ts = Vec::new();
        while ts.len() < 4 {
            let rem = 4 - ts.len();
            let n = inlet
                .pull_chunk_i16(&mut got, &mut ts, rem, Duration::from_secs(2))
                .unwrap();
            assert!(n > 0);
        }
        assert_eq!(got, want);
    }

    // Int8
    {
        let mut info = StreamInfo::new("FlatI8", "EEG", channels as u32, Format::Int8, rate);
        info.source_id = "flat-i8".to_string();
        let outlet = Outlet::new(info).unwrap();
        let mut inlet =
            Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 1024).unwrap();
        assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

        let want: Vec<i8> = (-8..8).collect();
        outlet.push_chunk_i8(&want, 500.0);

        let mut got = Vec::new();
        let mut ts = Vec::new();
        while ts.len() < 4 {
            let rem = 4 - ts.len();
            let n = inlet
                .pull_chunk_i8(&mut got, &mut ts, rem, Duration::from_secs(2))
                .unwrap();
            assert!(n > 0);
        }
        assert_eq!(got, want);
    }

    // Int64
    {
        let mut info = StreamInfo::new("FlatI64", "EEG", channels as u32, Format::Int64, rate);
        info.source_id = "flat-i64".to_string();
        let outlet = Outlet::new(info).unwrap();
        let mut inlet =
            Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 1024).unwrap();
        assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

        let want: Vec<i64> = (0..16)
            .map(|i| (i as i64) * 1_000_000_000_000 - 4_000_000_000_000)
            .collect();
        outlet.push_chunk_i64(&want, 600.0);

        let mut got = Vec::new();
        let mut ts = Vec::new();
        while ts.len() < 4 {
            let rem = 4 - ts.len();
            let n = inlet
                .pull_chunk_i64(&mut got, &mut ts, rem, Duration::from_secs(2))
                .unwrap();
            assert!(n > 0);
        }
        assert_eq!(got, want);
    }
}

#[test]
fn three_consumers_receive_identical_shared_samples_with_zero_loss() {
    let channels = 64usize;
    let mut info = StreamInfo::new(
        "SharedFanout",
        "EEG",
        channels as u32,
        Format::Float32,
        1000.0,
    );
    info.source_id = "shared-fanout-3".to_string();
    let outlet = Outlet::new(info).unwrap();

    let mut inlets: Vec<Inlet> = (0..3)
        .map(|_| Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 4096).unwrap())
        .collect();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while outlet.consumer_count() < 3 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(outlet.consumer_count(), 3);

    let total_samples = 256usize;
    let chunk_samples = 32usize;
    let mut expected = Vec::with_capacity(total_samples * channels);
    for c in 0..(total_samples / chunk_samples) {
        let mut buf = Vec::with_capacity(chunk_samples * channels);
        for s in 0..chunk_samples {
            let idx = c * chunk_samples + s;
            for ch in 0..channels {
                buf.push((idx as f32) + (ch as f32) * 0.01);
            }
        }
        expected.extend_from_slice(&buf);
        outlet.push_chunk_f32(&buf, 10.0 + ((c + 1) * chunk_samples) as f64 / 1000.0);
    }

    for (k, inlet) in inlets.iter_mut().enumerate() {
        let mut got = Vec::with_capacity(expected.len());
        let mut ts = Vec::with_capacity(total_samples);
        let read_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while ts.len() < total_samples && std::time::Instant::now() < read_deadline {
            let rem = total_samples - ts.len();
            let _ = inlet
                .pull_chunk_f32(&mut got, &mut ts, rem, Duration::from_millis(200))
                .unwrap();
        }
        assert_eq!(ts.len(), total_samples, "consumer {k} sample count");
        assert_eq!(got, expected, "consumer {k} values");
        assert_eq!(inlet.samples_lost(), Some(0), "consumer {k} lost");
    }
}

#[test]
#[ignore]
fn bench_flat_vs_value_and_fanout_1_vs_3() {
    use labstream_net::queue::Fanout;
    use labstream_wire::{PackedSample, Sample, Value};
    use std::time::Instant;

    let channels = 64usize;
    let total_samples = 4000 * 5; // 5 seconds at 4000 Hz
    let chunk = 32usize;
    let chunks = total_samples / chunk;

    // 1. Fanout push cost: legacy Sample vs PackedSample, 1 consumer vs 3 consumers.
    let raw_f32: Vec<f32> = (0..channels).map(|c| c as f32 * 0.1).collect();
    let sample_chunk: Vec<Sample> = (0..chunk)
        .map(|i| Sample {
            timestamp: 1.0 + i as f64 / 4000.0,
            values: raw_f32.iter().map(|&v| Value::F32(v)).collect(),
        })
        .collect();
    let packed_chunk: Vec<PackedSample> = (0..chunk)
        .map(|i| PackedSample::from_f32(1.0 + i as f64 / 4000.0, &raw_f32))
        .collect();

    for n_cons in [1usize, 3usize] {
        let fan = Fanout::with_rate(4000.0, labstream_net::history::HistoryLimits::DEFAULT);
        fan.start_history();
        let _qs: Vec<_> = (0..n_cons).map(|_| fan.add(1024)).collect();
        let t0 = Instant::now();
        for _ in 0..chunks {
            fan.push_many(&sample_chunk);
        }
        let legacy_ms = t0.elapsed().as_secs_f64() * 1e3;

        let fan_p = Fanout::with_rate(4000.0, labstream_net::history::HistoryLimits::DEFAULT);
        fan_p.start_history();
        let _qsp: Vec<_> = (0..n_cons).map(|_| fan_p.add(1024)).collect();
        let t1 = Instant::now();
        for _ in 0..chunks {
            fan_p.push_many_packed(&packed_chunk);
        }
        let packed_ms = t1.elapsed().as_secs_f64() * 1e3;
        eprintln!(
            "fanout 4000Hzx64ch ({n_cons} consumers, 5s): legacy={legacy_ms:.2} ms, packed={packed_ms:.2} ms"
        );
    }

    // 2. End-to-end Rust API decode/pull cost: pull_chunk (Vec<Sample>) vs pull_chunk_f32 (flat).
    let mut info = StreamInfo::new("BenchFlat", "EEG", channels as u32, Format::Float32, 4000.0);
    info.source_id = "bench-flat".to_string();
    let outlet = Outlet::new(info).unwrap();
    let mut inlet = Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 65536).unwrap();
    assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

    let chunk_f32: Vec<f32> = (0..chunk * channels).map(|i| i as f32 * 0.01).collect();
    // Legacy pull_chunk (allocating Vec<Sample> with 64 Values per sample)
    let mut sample_out = Vec::with_capacity(chunk);
    let t0 = Instant::now();
    let mut got_legacy = 0usize;
    for c in 0..chunks {
        outlet.push_chunk_f32(&chunk_f32, 10.0 + ((c + 1) * chunk) as f64 / 4000.0);
        while got_legacy < (c + 1) * chunk {
            sample_out.clear();
            got_legacy += inlet
                .pull_chunk(&mut sample_out, chunk, Duration::from_millis(50))
                .unwrap();
        }
    }
    let legacy_pull_ms = t0.elapsed().as_secs_f64() * 1e3;

    // Flat pull_chunk_f32 (zero Vec<Value> allocation)
    let mut f32_out = Vec::with_capacity(chunk * channels);
    let mut ts_out = Vec::with_capacity(chunk);
    let t1 = Instant::now();
    let mut got_flat = 0usize;
    for c in 0..chunks {
        outlet.push_chunk_f32(&chunk_f32, 20.0 + ((c + 1) * chunk) as f64 / 4000.0);
        while got_flat < (c + 1) * chunk {
            f32_out.clear();
            ts_out.clear();
            got_flat += inlet
                .pull_chunk_f32(&mut f32_out, &mut ts_out, chunk, Duration::from_millis(50))
                .unwrap();
        }
    }
    let flat_pull_ms = t1.elapsed().as_secs_f64() * 1e3;
    eprintln!(
        "inlet pull 4000Hzx64ch (5s): legacy pull_chunk={legacy_pull_ms:.2} ms, flat pull_chunk_f32={flat_pull_ms:.2} ms (speedup={:.2}x)",
        legacy_pull_ms / flat_pull_ms
    );
}
