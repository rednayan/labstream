//! The `dgram/1` feature from end to end, with a real inlet.
//! `docs/extensions.md`, section `dgram`.
//!
//! `dgram` is off by default, so this file is its own test process with
//! `labstream.Features = {seq, resume, dgram}`.

mod resume_common;

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use resume_common::{index, pull_until, run_dropout, use_configuration, DropSpec};
use std::sync::Once;
use std::time::{Duration, Instant};

static CONFIG: Once = Once::new();

fn setup() {
    CONFIG.call_once(|| {
        use_configuration(
            "dgram-live",
            "[labstream]\nFeatures = {seq, resume, dgram}\n",
        )
    });
}

fn wait_for_udp(inlet: &mut Inlet, out: &Outlet, next: &mut usize) -> bool {
    // A sample moves the session past the hello.
    let end = Instant::now() + Duration::from_secs(3);
    while !inlet.on_udp() && Instant::now() < end {
        push_one(out, *next, 1.0 + *next as f64 / 512.0);
        *next += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
    inlet.on_udp()
}

fn push_one(out: &Outlet, k: usize, t: f64) {
    out.push(&Sample {
        timestamp: t,
        values: vec![Value::F32(k as f32)],
    });
}

#[test]
fn the_samples_arrive_on_udp_in_order_with_their_timestamps() {
    setup();
    let mut info = StreamInfo::new("DgramLive1", "DgramTest", 1, Format::Float32, 512.0);
    info.source_id = "DgramLive1_src".to_string();
    let Ok(out) = Outlet::new(info) else {
        eprintln!("no local network; skipping");
        return;
    };
    let mut inlet =
        Inlet::open_recovering(out.info(), Duration::from_secs(5), 100_000).expect("an inlet");
    assert!(out.wait_for_consumers(Duration::from_secs(5)));
    let mut next = 0;
    assert!(
        wait_for_udp(&mut inlet, &out, &mut next),
        "the inlet uses UDP"
    );
    let first_on_udp = next;
    for k in next..next + 1000 {
        push_one(&out, k, 1.0 + k as f64 / 512.0);
        if k % 8 == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let last = first_on_udp + 999;
    let mut got = Vec::new();
    pull_until(&mut inlet, &mut got, last, Duration::from_secs(5));
    let values: Vec<usize> = got.iter().map(index).collect();
    assert_eq!(
        values,
        (0..=last).collect::<Vec<_>>(),
        "each sample once, in order"
    );
    for s in &got {
        let k = index(s);
        assert_eq!(s.timestamp, 1.0 + k as f64 / 512.0, "sample {k}");
    }
    assert_eq!(inlet.samples_lost(), Some(0));
    assert!(inlet.on_udp());
}

#[test]
fn large_samples_go_on_tcp_and_stay_in_order() {
    setup();
    let mut info = StreamInfo::new("DgramLive2", "DgramTest", 1, Format::String, 0.0);
    info.source_id = "DgramLive2_src".to_string();
    let Ok(out) = Outlet::new(info) else {
        eprintln!("no local network; skipping");
        return;
    };
    let mut inlet =
        Inlet::open_recovering(out.info(), Duration::from_secs(5), 100_000).expect("an inlet");
    assert!(out.wait_for_consumers(Duration::from_secs(5)));
    // Each 10th marker has 2000 bytes, too large for a datagram. The text
    // starts with the index, and the rest is padding.
    let marker = |k: usize| {
        let mut text = k.to_string();
        if k % 10 == 0 {
            text.push(' ');
            text.push_str(&"x".repeat(2000));
        }
        Sample {
            timestamp: 0.0,
            values: vec![Value::Str(text.into_bytes())],
        }
    };
    let end = Instant::now() + Duration::from_secs(3);
    let mut k = 0;
    while !inlet.on_udp() && Instant::now() < end {
        out.push(&marker(k));
        k += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(inlet.on_udp(), "the inlet uses UDP");
    for j in k..k + 200 {
        out.push(&marker(j));
        std::thread::sleep(Duration::from_millis(1));
    }
    let last = k + 199;
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let _ = inlet.pull_chunk(&mut got, 100_000, Duration::from_millis(20));
        let done = got.last().map(|s| match &s.values[0] {
            Value::Str(b) => String::from_utf8_lossy(b)
                .split(' ')
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            _ => 0,
        });
        if done == Some(last) {
            break;
        }
    }
    let values: Vec<usize> = got
        .iter()
        .map(|s| match &s.values[0] {
            Value::Str(b) => String::from_utf8_lossy(b)
                .split(' ')
                .next()
                .unwrap()
                .parse()
                .unwrap(),
            _ => panic!("a string"),
        })
        .collect();
    assert_eq!(values, (0..=last).collect::<Vec<_>>());
    assert_eq!(inlet.samples_lost(), Some(0));
}

#[test]
fn a_cut_connection_comes_back_with_no_lost_sample() {
    setup();
    let spec = DropSpec {
        name: "DgramDrop1",
        format: Format::Float32,
        channels: 8,
        rate: 512.0,
        chunk: 8,
        cut: Duration::from_secs(3),
        post: false,
    };
    let Some(run) = run_dropout(&spec) else {
        eprintln!("no local network; skipping");
        return;
    };
    assert!(run.recoveries >= 1, "the inlet connected again");
    let first = index(&run.got[0]);
    let values: Vec<usize> = run.got.iter().map(index).collect();
    let seen: std::collections::BTreeSet<usize> = values.iter().copied().collect();
    let missing: Vec<usize> = (first..=run.last).filter(|k| !seen.contains(k)).collect();
    let back = values.windows(2).filter(|w| w[1] <= w[0]).count();
    assert!(
        missing.is_empty() && back == 0,
        "{} samples, {} missing from {:?} to {:?}, {back} out of order, lost {:?}, resent {:?}",
        values.len(),
        missing.len(),
        missing.first(),
        missing.last(),
        run.lost,
        run.resent
    );
    assert_eq!(run.lost, Some(0));
}
