//! Live integration tests for Step 7: lossless wire compression (`comp/1`).
//!
//! Each test disables `shm` via `labstream_net::config::set_content` so the
//! connection runs over TCP or UDP (`dgram/1`), exercising both the stateful
//! TCP `comp/1` path and the self-contained UDP `DATA_COMP` path.

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_proto::features::SelectParam;
use labstream_wire::{Format, PackedSample};
use std::time::{Duration, Instant};

fn init_features_no_shm() {
    // Runs before any `config::get()` in this test binary.
    labstream_net::config::set_content(
        "[labstream]\nFeatures = {seq, resume, dgram, select, comp}\n",
    );
}

fn realistic_int32_eeg_rows(channels: usize, count: usize) -> Vec<Vec<i32>> {
    let mut rng = 0x5EED_C0DE_1234_5678u64;
    let mut state = vec![0i32; channels];
    for (c, s) in state.iter_mut().enumerate() {
        *s = 150_000 + (c as i32) * 12_345;
    }
    let mut rows = Vec::with_capacity(count);
    for t in 0..count {
        let mut row = Vec::with_capacity(channels);
        for (c, st) in state.iter_mut().enumerate() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let step = ((rng % 37) as i32) - 18 + (((t + c) % 7) as i32 - 3);
            *st = st.wrapping_add(step);
            row.push(*st);
        }
        rows.push(row);
    }
    rows
}

#[test]
fn comp_round_trips_eeg_over_udp_and_reduces_wire_bytes_over_50_pct() {
    init_features_no_shm();
    let channels = 32;
    let info = StreamInfo::new("CompUdpEeg", "EEG", channels as u32, Format::Int32, 512.0);
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live comp test (no local network): {e}");
            return;
        }
    };
    let pub_info = outlet.info().clone();
    let mut inlet = Inlet::open(&pub_info, Duration::from_secs(5)).expect("open inlet");
    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));
    assert!(!inlet.on_shm(), "shm is disabled in this test binary");
    assert!(
        inlet.is_compressed(),
        "comp/1 must negotiate on numeric stream"
    );

    // Wait briefly so UDP hello finishes and the path switches to UDP.
    let rows = realistic_int32_eeg_rows(channels, 512);
    let mut expected = Vec::with_capacity(rows.len());
    for chunk_idx in 0..(rows.len() / 16) {
        let base = chunk_idx * 16;
        let mut flat = Vec::with_capacity(16 * channels);
        for k in 0..16 {
            let idx = base + k;
            let t = 5000.0 + idx as f64 / 512.0;
            flat.extend_from_slice(&rows[idx]);
            expected.push((t, rows[idx].clone()));
        }
        let end_ts = 5000.0 + (base + 15) as f64 / 512.0;
        outlet.push_chunk_i32(&flat, end_ts);
        if chunk_idx == 0 {
            std::thread::sleep(Duration::from_millis(60));
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    let mut received: Vec<PackedSample> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while received.len() < expected.len() && Instant::now() < deadline {
        inlet
            .pull_chunk_packed(&mut received, 256, Duration::from_millis(100))
            .expect("pull chunk");
    }
    assert_eq!(received.len(), expected.len());
    assert_eq!(inlet.samples_lost(), Some(0));
    assert!(inlet.on_udp(), "expected UDP path with DATA_COMP");

    for (i, (got, (exp_ts, exp_vals))) in received.iter().zip(expected.iter()).enumerate() {
        assert_eq!(got.timestamp.to_bits(), exp_ts.to_bits(), "sample {i} ts");
        let mut vals = Vec::with_capacity(channels);
        got.push_into_i32(&mut vals);
        assert_eq!(vals, *exp_vals, "sample {i} values");
    }

    let ratio = inlet
        .compression_ratio()
        .expect("compression ratio available");
    let health = inlet.health();
    assert!(health.compressed);
    assert_eq!(health.compression_ratio, Some(ratio));
    assert!(
        ratio < 0.50,
        "expected >50% reduction on 32-ch Int32 EEG over UDP, got ratio={ratio:.3}"
    );
}

#[test]
fn comp_round_trips_tcp_bit_exact_with_special_floats_and_channel_selection() {
    init_features_no_shm();
    // 128 Float32 channels = 521 bytes/sample (> 400 B MAX_SAMPLE_BYTES),
    // which forces TCP while keeping `comp/1` active.
    let channels = 128;
    let info = StreamInfo::new(
        "CompTcpF32",
        "EEG",
        channels as u32,
        Format::Float32,
        1000.0,
    );
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live comp test (no local network): {e}");
            return;
        }
    };
    let pub_info = outlet.info().clone();
    let mut inlet = Inlet::open(&pub_info, Duration::from_secs(5)).expect("open tcp inlet");
    // Also connect a second consumer using `select/1` (8 channels) + `comp/1`.
    let sel_chs: Vec<u16> = vec![0, 1, 2, 3, 64, 65, 126, 127];
    let mut sel_inlet = Inlet::open_selected(
        &pub_info,
        Duration::from_secs(5),
        1024,
        false,
        SelectParam::channels(sel_chs.clone()),
    )
    .expect("open selected inlet");

    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));
    assert!(inlet.is_compressed());
    assert!(!inlet.on_udp(), "128 ch f32 stays on TCP");
    assert!(sel_inlet.is_compressed());

    let mut expected_full: Vec<(f64, Vec<u32>)> = Vec::new();
    for i in 0..120usize {
        let ts = 2000.0 + i as f64 / 1000.0;
        let mut row = Vec::with_capacity(channels);
        for c in 0..channels {
            let v: f32 = match (i, c) {
                (5, 0) => -0.0f32,
                (6, 0) => 0.0f32,
                (7, 1) => f32::from_bits(0x7FC0_0042),
                (8, 2) => f32::from_bits(0xFFC0_1337),
                (9, 3) => f32::INFINITY,
                (10, 3) => f32::NEG_INFINITY,
                _ => 1000.0 + (c as f32) * 10.0 + (i as f32) * 0.25,
            };
            row.push(v);
        }
        let bits: Vec<u32> = row.iter().map(|v| v.to_bits()).collect();
        expected_full.push((ts, bits));
        outlet.push_packed(PackedSample::from_f32(ts, &row));
    }

    let mut got_full = Vec::new();
    let mut got_sel = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (got_full.len() < expected_full.len() || got_sel.len() < expected_full.len())
        && Instant::now() < deadline
    {
        if got_full.len() < expected_full.len() {
            inlet
                .pull_chunk_packed(&mut got_full, 256, Duration::from_millis(50))
                .expect("pull full");
        }
        if got_sel.len() < expected_full.len() {
            sel_inlet
                .pull_chunk_packed(&mut got_sel, 256, Duration::from_millis(50))
                .expect("pull sel");
        }
    }
    assert_eq!(got_full.len(), expected_full.len());
    assert_eq!(got_sel.len(), expected_full.len());

    for (i, (s, (exp_ts, exp_bits))) in got_full.iter().zip(expected_full.iter()).enumerate() {
        assert_eq!(s.timestamp.to_bits(), exp_ts.to_bits(), "full {i} ts");
        let bytes = s.as_numeric_bytes(Format::Float32).unwrap();
        let bits: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(bits, *exp_bits, "full {i} exact f32 bits");
    }

    for (i, (s, (exp_ts, exp_bits))) in got_sel.iter().zip(expected_full.iter()).enumerate() {
        assert_eq!(s.timestamp.to_bits(), exp_ts.to_bits(), "sel {i} ts");
        let bytes = s.as_numeric_bytes(Format::Float32).unwrap();
        let bits: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let want: Vec<u32> = sel_chs.iter().map(|&idx| exp_bits[idx as usize]).collect();
        assert_eq!(bits, want, "sel {i} exact f32 bits");
    }

    let ratio = inlet.compression_ratio().expect("tcp compression ratio");
    assert!(
        ratio < 0.60,
        "expected >40% TCP compression on smooth 128-ch f32 stream, got ratio={ratio:.3}"
    );
}

#[test]
fn comp_stays_off_for_string_marker_streams() {
    init_features_no_shm();
    let info = StreamInfo::new("CompStrings", "Markers", 1, Format::String, 0.0);
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live comp test (no local network): {e}");
            return;
        }
    };
    let pub_info = outlet.info().clone();
    let mut inlet = Inlet::open(&pub_info, Duration::from_secs(5)).expect("open string inlet");
    assert!(
        !inlet.is_compressed(),
        "comp/1 must not activate on strings"
    );
    assert_eq!(inlet.compression_ratio(), None);

    outlet.push_packed(PackedSample::from_strings(12.5, vec![b"stimulus".to_vec()]));
    let got = inlet
        .pull_packed(Duration::from_secs(2))
        .expect("pull")
        .expect("sample");
    assert_eq!(got.timestamp, 12.5);
}
