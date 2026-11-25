//! Live tests for step 16 (`meta/1` in-band live metadata).

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_wire::Format;
use std::time::{Duration, Instant};

#[test]
fn live_metadata_arrives_in_order_with_exact_timestamps_and_seq() {
    let mut info = StreamInfo::new("MetaEeg", "EEG", 2, Format::Float32, 250.0);
    info.source_id = format!("meta-live-{}", std::process::id());
    let outlet = Outlet::new(info).expect("outlet");
    let mut inlet =
        Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 4096).expect("inlet");
    assert!(outlet.wait_for_consumers(Duration::from_secs(2)));
    assert!(inlet.has_live_metadata(), "meta/1 must negotiate");

    // Emit initial impedance, push 10 samples, emit updated impedance, push 10 more.
    let u0 = outlet.push_metadata_at("impedance/Fp1", "12.5 kOhm", 100.0);
    assert_eq!(u0.seq, 0);

    let mut data1 = Vec::with_capacity(20);
    for i in 0..10 {
        data1.push(i as f32);
        data1.push(-(i as f32));
    }
    outlet.push_chunk_f32(&data1, 100.04);

    let u1 = outlet.push_metadata_at("impedance/Fp1", "4.2 kOhm", 100.05);
    assert_eq!(u1.seq, 10);
    let u2 = outlet.push_metadata_at("channel/1/status", "bad", 100.05);
    assert_eq!(u2.seq, 10);

    let mut data2 = Vec::with_capacity(20);
    for i in 10..20 {
        data2.push(i as f32);
        data2.push(-(i as f32));
    }
    outlet.push_chunk_f32(&data2, 100.08);

    // Receive all 20 samples.
    let mut got_vals = Vec::new();
    let mut got_ts = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while got_ts.len() < 20 && Instant::now() < deadline {
        let rem = 20 - got_ts.len();
        let _ = inlet
            .pull_chunk_f32(&mut got_vals, &mut got_ts, rem, Duration::from_millis(50))
            .unwrap();
    }
    assert_eq!(got_ts.len(), 20);

    // Receive all 3 metadata updates in exact order.
    let mut updates = Vec::new();
    let meta_deadline = Instant::now() + Duration::from_secs(2);
    while updates.len() < 3 && Instant::now() < meta_deadline {
        updates.extend(inlet.pull_metadata());
        if updates.len() < 3 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(updates.len(), 3, "got updates: {updates:?}");
    assert_eq!(updates[0].key, "impedance/Fp1");
    assert_eq!(updates[0].value, "12.5 kOhm");
    assert_eq!(updates[0].seq, 0);
    assert_eq!(updates[0].timestamp.to_bits(), 100.0f64.to_bits());

    assert_eq!(updates[1].key, "impedance/Fp1");
    assert_eq!(updates[1].value, "4.2 kOhm");
    assert_eq!(updates[1].seq, 10);
    assert_eq!(updates[1].timestamp.to_bits(), 100.05f64.to_bits());

    assert_eq!(updates[2].key, "channel/1/status");
    assert_eq!(updates[2].value, "bad");
    assert_eq!(updates[2].seq, 10);

    // Latest snapshot reflects the updated impedance and channel status.
    let snap = inlet.metadata();
    assert_eq!(snap.get("impedance/Fp1").unwrap().value, "4.2 kOhm");
    assert_eq!(snap.get("channel/1/status").unwrap().value, "bad");
}

#[test]
fn late_joining_inlet_receives_initial_metadata_snapshot() {
    let mut info = StreamInfo::new("MetaSnap", "EEG", 1, Format::Float32, 100.0);
    info.source_id = format!("meta-snap-{}", std::process::id());
    let outlet = Outlet::new(info).expect("outlet");

    outlet.push_metadata_at("impedance/Cz", "3.1 kOhm", 50.0);
    outlet.push_chunk_f32(&[1.0, 2.0, 3.0], 50.03);

    // Connect after the metadata was already emitted.
    let inlet =
        Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 1024).expect("inlet");
    assert!(inlet.has_live_metadata());

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut updates = Vec::new();
    while updates.is_empty() && Instant::now() < deadline {
        updates.extend(inlet.pull_metadata());
        if updates.is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].key, "impedance/Cz");
    assert_eq!(updates[0].value, "3.1 kOhm");
    assert_eq!(updates[0].timestamp.to_bits(), 50.0f64.to_bits());
}

#[test]
fn string_stream_on_tcp_delivers_live_metadata() {
    let mut info = StreamInfo::new("MetaMarkers", "Markers", 1, Format::String, 0.0);
    info.source_id = format!("meta-str-{}", std::process::id());
    let outlet = Outlet::new(info).expect("outlet");
    let mut inlet =
        Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 1024).expect("inlet");
    assert!(outlet.wait_for_consumers(Duration::from_secs(2)));
    assert!(inlet.has_live_metadata());
    assert!(!inlet.on_shm());

    outlet.push_metadata_at("session/phase", "baseline", 10.0);
    outlet.push(&labstream_wire::Sample {
        timestamp: 10.1,
        values: vec![labstream_wire::Value::Str(b"start".to_vec())],
    });
    outlet.push_metadata_at("session/phase", "task", 10.2);

    let s = inlet
        .pull(Duration::from_secs(2))
        .unwrap()
        .expect("marker sample");
    assert_eq!(s.values[0], labstream_wire::Value::Str(b"start".to_vec()));

    let mut updates = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while updates.len() < 2 && Instant::now() < deadline {
        updates.extend(inlet.pull_metadata());
        if updates.len() < 2 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].value, "baseline");
    assert_eq!(updates[0].seq, 0);
    assert_eq!(updates[1].value, "task");
    assert_eq!(updates[1].seq, 1);
}
