//! Live tests for step 4: local-to-network LSL stream relay (`docs/roadmap.md`).

mod resume_common;

use labstream_net::desc::Node;
use labstream_net::relay::{is_relayed, RelayOptions, StreamRelay, RELAY_DESC_TAG};
use labstream_net::{read_fullinfo, Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use resume_common::Proxy;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[test]
fn relay_preserves_description_exact_bits_and_timestamps_and_is_distinguishable() {
    let mut info = StreamInfo::new("DeviceEEG", "EEG", 4, Format::Float32, 250.0);
    info.source_id = "dev-eeg-01".to_string();
    info.desc.name = "desc".to_string();
    let mut channels = Node::element("channels");
    for label in ["Fp1", "Fp2", "C3", "C4"] {
        let mut ch = Node::element("channel");
        let mut l = Node::element("label");
        l.children.push(Node::text(label));
        let mut u = Node::element("unit");
        u.children.push(Node::text("microvolts"));
        ch.children.push(l);
        ch.children.push(u);
        channels.children.push(ch);
    }
    info.desc.children.push(channels);

    let source = Outlet::new(info).expect("source outlet");
    assert!(!is_relayed(source.info()));

    let relay = StreamRelay::start(source.info(), RelayOptions::default()).expect("relay starts");
    assert!(source.wait_for_consumers(Duration::from_secs(2)));

    // Criterion 3: inlet can tell relayed stream from source stream on both
    // short discovery info and fullinfo.
    assert!(is_relayed(relay.info()));
    assert_eq!(relay.info().name, "DeviceEEG-relay");
    assert_eq!(relay.info().source_id, "dev-eeg-01-relay");
    assert_ne!(relay.info().uid, source.info().uid);

    // Criterion 2: full description preserves all source metadata and channel tree.
    let full = read_fullinfo(relay.info(), Duration::from_secs(2)).expect("fullinfo");
    assert_eq!(full.stream_type, "EEG");
    assert_eq!(full.channel_count, 4);
    assert_eq!(full.format, Format::Float32);
    assert_eq!(full.nominal_srate, 250.0);
    assert!(full.desc.children.iter().any(|c| c.name == "channels"));
    assert!(full.desc.children.iter().any(|c| c.name == RELAY_DESC_TAG));
    let ch_node = full
        .desc
        .children
        .iter()
        .find(|c| c.name == "channels")
        .unwrap();
    assert_eq!(ch_node.children.len(), 4);

    // Push samples with exact bits (including negative zero and subnormals) and exact timestamps.
    let mut inlet =
        Inlet::open_with_buffer(relay.info(), Duration::from_secs(2), 1024).expect("inlet");
    let wait_down = Instant::now() + Duration::from_secs(2);
    while relay.consumer_count() == 0 && Instant::now() < wait_down {
        std::thread::sleep(Duration::from_millis(10));
    }

    let sent = vec![
        Sample {
            timestamp: 1_234.567_89,
            values: vec![
                Value::F32(-0.0),
                Value::F32(1.25),
                Value::F32(-42.5),
                Value::F32(f32::MAX),
            ],
        },
        Sample {
            timestamp: 1_234.571_89,
            values: vec![
                Value::F32(0.0),
                Value::F32(f32::MIN),
                Value::F32(std::f32::consts::PI),
                Value::F32(-1e-20),
            ],
        },
    ];
    source.push_samples(sent.clone());

    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while got.len() < sent.len() && Instant::now() < deadline {
        let rem = sent.len() - got.len();
        let _ = inlet
            .pull_chunk(&mut got, rem, Duration::from_millis(100))
            .unwrap();
    }
    assert_eq!(got.len(), sent.len());
    for (a, b) in got.iter().zip(&sent) {
        assert_eq!(a.timestamp.to_bits(), b.timestamp.to_bits());
        assert_eq!(a.values.len(), b.values.len());
        for (va, vb) in a.values.iter().zip(&b.values) {
            match (va, vb) {
                (Value::F32(x), Value::F32(y)) => assert_eq!(x.to_bits(), y.to_bits()),
                _ => panic!("unexpected format"),
            }
        }
    }
}

#[test]
fn cut_after_relay_recovers_with_zero_loss() {
    let rate = 512.0;
    let channels = 8usize;
    let mut info = StreamInfo::new("CutRelaySrc", "EEG", channels as u32, Format::Float32, rate);
    info.source_id = "cut-relay-src".to_string();

    let source = Outlet::new(info).expect("source outlet");
    let relay = StreamRelay::start(source.info(), RelayOptions::default()).expect("relay starts");
    assert!(source.wait_for_consumers(Duration::from_secs(2)));

    // Put the cuttable proxy on the link AFTER the relay.
    let proxy = Proxy::start(relay.info().v4data_port).expect("proxy binds");
    let via_proxy = proxy.info(relay.info());
    let mut inlet =
        Inlet::open_recovering(&via_proxy, Duration::from_secs(5), 32_768).expect("inlet");

    let push_block = |from: usize, to: usize| {
        let mut buf = Vec::with_capacity((to - from) * channels);
        for i in from..to {
            buf.push(i as f32);
            for c in 1..channels {
                buf.push(c as f32);
            }
        }
        source.push_chunk_f32(&buf, 500.0 + (to - 1) as f64 / rate);
    };

    // Warmup before the cut.
    push_block(0, 128);
    let mut got_vals = Vec::new();
    let mut got_ts = Vec::new();
    let warm_end = Instant::now() + Duration::from_secs(3);
    while got_ts.len() < 128 && Instant::now() < warm_end {
        let rem = 128 - got_ts.len();
        let _ = inlet
            .pull_chunk_f32(&mut got_vals, &mut got_ts, rem, Duration::from_millis(100))
            .unwrap();
    }
    assert_eq!(got_ts.len(), 128);

    // Cut the link after the relay while the source keeps pushing 2 seconds of data.
    proxy.paused.store(true, Ordering::SeqCst);
    proxy.cut();
    let cut_wait = Instant::now() + Duration::from_secs(2);
    while inlet.is_connected() && Instant::now() < cut_wait {
        std::thread::sleep(Duration::from_millis(10));
    }

    let during_cut = 1024usize;
    for chunk_start in (128..128 + during_cut).step_by(64) {
        push_block(chunk_start, chunk_start + 64);
        std::thread::sleep(Duration::from_millis(2));
    }
    // Wait briefly so the relay pulls all cut samples from the local source.
    let fwd_wait = Instant::now() + Duration::from_secs(2);
    while relay.forwarded() < (128 + during_cut) as u64 && Instant::now() < fwd_wait {
        std::thread::sleep(Duration::from_millis(10));
    }

    // Restore link after relay and push tail.
    proxy.paused.store(false, Ordering::SeqCst);
    let tail_start = 128 + during_cut;
    let total = tail_start + 128;
    push_block(tail_start, total);

    let recv_end = Instant::now() + Duration::from_secs(10);
    while got_ts.len() < total && Instant::now() < recv_end {
        let rem = total - got_ts.len();
        let _ = inlet
            .pull_chunk_f32(&mut got_vals, &mut got_ts, rem, Duration::from_millis(200))
            .unwrap();
    }
    assert_eq!(got_ts.len(), total, "must receive every sample across cut");
    for i in 0..total {
        assert_eq!(got_vals[i * channels] as usize, i, "sample index {i}");
    }
    assert_eq!(inlet.samples_lost(), Some(0), "Criterion 1: zero lost");
    assert!(
        inlet.samples_resent().unwrap_or(0) >= 512,
        "expected resent samples from relay history, got {:?}",
        inlet.samples_resent()
    );
}
