//! The watchdog of the inlet with `dgram/1`. `docs/extensions.md`, section
//! `dgram`, conformance item 4b.
//!
//! On UDP, no byte arrives on TCP. The datagrams are the data, so the
//! watchdog must not end the UDP part of the connection. This file is its
//! own test process, with a watchdog time of 1.5 s.

mod resume_common;

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use resume_common::{index, pull_until, use_configuration};
use std::time::{Duration, Instant};

#[test]
fn a_connection_on_udp_keeps_its_samples_past_the_watchdog_time() {
    use_configuration(
        "dgram-watchdog",
        "[labstream]\nFeatures = {seq, resume, dgram}\n[tuning]\nWatchdogTimeThreshold = 1.5\n",
    );
    let mut info = StreamInfo::new("DgramWatchdog", "DgramTest", 1, Format::Float32, 200.0);
    info.source_id = "DgramWatchdog_src".to_string();
    let Ok(out) = Outlet::new(info) else {
        eprintln!("no local network; skipping");
        return;
    };
    let mut inlet =
        Inlet::open_recovering(out.info(), Duration::from_secs(5), 100_000).expect("an inlet");
    assert!(out.wait_for_consumers(Duration::from_secs(5)));
    let push = |k: usize| {
        out.push(&Sample {
            timestamp: 1.0 + k as f64 / 200.0,
            values: vec![Value::F32(k as f32)],
        })
    };
    let mut k = 0;
    let end = Instant::now() + Duration::from_secs(3);
    while !inlet.on_udp() && Instant::now() < end {
        push(k);
        k += 1;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(inlet.on_udp(), "the inlet uses UDP");

    // 4 s on UDP: more than twice the watchdog time.
    let mut got = Vec::new();
    let end = Instant::now() + Duration::from_secs(4);
    while Instant::now() < end {
        push(k);
        k += 1;
        std::thread::sleep(Duration::from_millis(5));
        let _ = inlet.pull_chunk(&mut got, 100_000, Duration::ZERO);
    }
    pull_until(&mut inlet, &mut got, k - 1, Duration::from_secs(3));
    let values: Vec<usize> = got.iter().map(index).collect();
    assert_eq!(
        values,
        (0..k).collect::<Vec<_>>(),
        "each sample once, in order"
    );
    assert!(inlet.on_udp(), "still on UDP");
    assert_eq!(inlet.samples_lost(), Some(0));
}
