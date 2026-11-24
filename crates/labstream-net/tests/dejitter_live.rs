//! Live and mock-server tests for step 2: loss-aware dejitter (`docs/roadmap.md`).

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_proto::features::{self, Token};
use labstream_proto::header::parse_block;
use labstream_proto::negotiate::{negotiate, FeedRequest, Outcome, ServerStream, LITTLE};
use labstream_proto::seq::Numbering;
use labstream_time::{flags, Dejitterer};
use labstream_wire::{ByteOrder, Codec, Format, Sample, Value, TEST_PATTERN_OFFSETS};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// Minimal mock LSL feed server that sends `before` samples at `srate` Hz,
/// drops `gap` samples, and then sends `after` samples. When `use_seq` is
/// true, it answers `seq/1` and emits sequence numbering records.
fn spawn_gap_server(
    uid: &str,
    srate: f64,
    t0: f64,
    before: usize,
    gap: usize,
    after: usize,
    use_seq: bool,
) -> Option<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    let uid = uid.to_string();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_nodelay(true);
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let nl = buf.iter().position(|&b| b == b'\n').unwrap();
        let req_line = String::from_utf8_lossy(&buf[..nl]).trim().to_string();
        let (ver, req_uid) = req_line
            .strip_prefix("LSL:streamfeed/")
            .map(|r| {
                let mut it = r.splitn(2, ' ');
                (
                    it.next().unwrap_or("110").trim().parse().unwrap_or(110),
                    it.next().unwrap_or("").trim().to_string(),
                )
            })
            .unwrap_or((110, String::new()));
        let (headers, _) = parse_block(&buf[nl + 1..]).unwrap_or_default();
        let srv = ServerStream::new(&uid, Format::Float32, LITTLE, 1000.0);
        let req = FeedRequest::from_headers(ver, &req_uid, &headers, srv.channel_bytes);
        let Outcome::Accept(accepted) = negotiate(&req, &srv) else {
            return;
        };
        let answered = if use_seq {
            Some(vec![Token::new(features::SEQ, 1)])
        } else {
            None
        };
        let hdr_wire = accepted.to_wire_with(srv.configured_version, &uid, answered.as_deref());
        let _ = stream.write_all(hdr_wire.as_bytes());

        let codec = Codec::new(Format::Float32, 1, ByteOrder::Little);
        let mut wire = Vec::new();
        for off in TEST_PATTERN_OFFSETS {
            let s = labstream_wire::test_pattern(Format::Float32, 1, off);
            codec.encode(&s, &mut wire).unwrap();
        }
        let _ = stream.write_all(&wire);
        let _ = stream.flush();

        let dt = 1.0 / srate;
        let mut payload = Vec::new();
        let mut num = Numbering::new();
        if use_seq {
            num.start(0, &mut payload);
        }
        for k in 0..before {
            if use_seq {
                num.before(k as u64, &mut payload);
            }
            let s = Sample {
                timestamp: t0 + k as f64 * dt,
                values: vec![Value::F32(k as f32)],
            };
            codec.encode(&s, &mut payload).unwrap();
        }
        for k in (before + gap)..(before + gap + after) {
            if use_seq {
                num.before(k as u64, &mut payload);
            }
            let s = Sample {
                timestamp: t0 + k as f64 * dt,
                values: vec![Value::F32(k as f32)],
            };
            codec.encode(&s, &mut payload).unwrap();
        }
        let _ = stream.write_all(&payload);
        let _ = stream.flush();
        // Keep socket open briefly while the inlet drains.
        std::thread::sleep(Duration::from_millis(400));
    });
    Some(port)
}

fn info_for_mock(name: &str, uid: &str, srate: f64, port: u16) -> StreamInfo {
    let mut info = StreamInfo::new(name, "Test", 1, Format::Float32, srate);
    info.uid = uid.to_string();
    info.v4address = "127.0.0.1".to_string();
    info.v4data_port = port;
    info
}

#[test]
fn seq_connection_keeps_dejitter_accurate_across_100_sample_gap() {
    let srate = 100.0;
    let dt = 1.0 / srate;
    let t0 = 5000.0;
    let uid = "dejitter-seq-100";
    let Some(port) = spawn_gap_server(uid, srate, t0, 200, 100, 20, true) else {
        eprintln!("no local network; skipping");
        return;
    };
    let info = info_for_mock("DejitterSeq", uid, srate, port);
    let mut inlet = Inlet::open_with_buffer(&info, Duration::from_secs(5), 1024).unwrap();
    inlet.set_postprocessing(flags::DEJITTER);

    // Pull all 220 samples (200 before gap, 20 after 100-sample gap).
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.len() < 220 && Instant::now() < deadline {
        let _ = inlet.pull_chunk(&mut got, 256, Duration::from_millis(50));
    }
    assert_eq!(got.len(), 220);
    assert_eq!(inlet.samples_lost(), Some(100));

    // Criterion 1: with zero loss (first 200 samples), bits match plain Dejitterer.
    let mut reference = Dejitterer::new(t0, srate, labstream_time::DEFAULT_SMOOTHING_HALFTIME);
    for (i, sample) in got.iter().enumerate().take(200) {
        let want = reference.dejitter(t0 + i as f64 * dt);
        assert_eq!(
            sample.timestamp.to_bits(),
            want.to_bits(),
            "zero-loss bit mismatch at sample {i}"
        );
    }

    // Criterion 2: first smoothed timestamp after the 100-sample gap is within
    // less than one sample interval of true time (`t0 + 300 * dt`).
    let true_after = t0 + 300.0 * dt;
    let err = (got[200].timestamp - true_after).abs();
    assert!(
        err < dt,
        "expected error < {dt} s after 100-sample gap, got {err} s (ts={}, want={true_after})",
        got[200].timestamp
    );
}

#[test]
fn plain_lsl_without_seq_matches_liblsl_dejitter_unchanged() {
    let srate = 100.0;
    let dt = 1.0 / srate;
    let t0 = 5000.0;
    let uid = "dejitter-plain-100";
    let Some(port) = spawn_gap_server(uid, srate, t0, 200, 100, 10, false) else {
        eprintln!("no local network; skipping");
        return;
    };
    let info = info_for_mock("DejitterPlain", uid, srate, port);
    let mut inlet = Inlet::open_with_buffer(&info, Duration::from_secs(5), 1024).unwrap();
    inlet.set_postprocessing(flags::DEJITTER);

    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.len() < 210 && Instant::now() < deadline {
        let _ = inlet.pull_chunk(&mut got, 256, Duration::from_millis(50));
    }
    assert_eq!(got.len(), 210);
    assert_eq!(inlet.samples_lost(), None);

    // Criterion 3: without `seq/1`, every timestamp matches liblsl bit-for-bit
    // (including the ~1 s lag after the unobserved 100-sample gap).
    let mut reference = Dejitterer::new(t0, srate, labstream_time::DEFAULT_SMOOTHING_HALFTIME);
    for (i, sample) in got.iter().enumerate() {
        let raw_idx = if i < 200 { i } else { i + 100 };
        let want = reference.dejitter(t0 + raw_idx as f64 * dt);
        assert_eq!(
            sample.timestamp.to_bits(),
            want.to_bits(),
            "plain-LSL dejitter changed at {i}"
        );
    }
}

#[test]
fn live_outlet_overflow_keeps_dejitter_aligned() {
    let srate = 1000.0;
    let dt = 1.0 / srate;
    let t0 = 2000.0;
    let name = format!("DejitterLive_{}", std::process::id());
    let info = StreamInfo::new(&name, "EEG", 1, Format::Float32, srate);
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping live dejitter test (no local network): {e}");
            return;
        }
    };
    let mut inlet = Inlet::open_recovering(outlet.info(), Duration::from_secs(5), 32).unwrap();
    inlet.set_postprocessing(flags::DEJITTER);
    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));

    // Warm up with 200 clean samples in small chunks so nothing drops yet.
    let mut seen = 0usize;
    for k in 0..200usize {
        outlet.push(&Sample {
            timestamp: t0 + k as f64 * dt,
            values: vec![Value::F32(k as f32)],
        });
        if k % 8 == 7 {
            let mut tmp = Vec::new();
            let _ = inlet.pull_chunk(&mut tmp, 64, Duration::from_millis(5));
            if let Some(last) = tmp.last() {
                if let Value::F32(v) = last.values[0] {
                    seen = v as usize;
                }
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while seen < 199 && Instant::now() < deadline {
        let mut tmp = Vec::new();
        let _ = inlet.pull_chunk(&mut tmp, 256, Duration::from_millis(20));
        if let Some(last) = tmp.last() {
            if let Value::F32(v) = last.values[0] {
                seen = v as usize;
            }
        }
    }
    assert_eq!(seen, 199);

    // Burst 100_000 samples so the 32-sample outlet queue + 30 s history lose
    // tens of thousands of samples, then verify the final dejittered timestamp
    // stays within 1 sample interval of its true timestamp.
    let n = 100_000usize;
    let block: Vec<Sample> = (200..200 + n)
        .map(|k| Sample {
            timestamp: t0 + k as f64 * dt,
            values: vec![Value::F32(k as f32)],
        })
        .collect();
    outlet.push_samples(block);

    let last_idx = 200 + n - 1;
    let mut final_sample: Option<Sample> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && final_sample.is_none() {
        let mut tmp = Vec::new();
        let _ = inlet.pull_chunk(&mut tmp, 100_000, Duration::from_millis(20));
        for s in tmp {
            if matches!(s.values[0], Value::F32(v) if v as usize == last_idx) {
                final_sample = Some(s);
                break;
            }
        }
    }
    let last = final_sample.expect("expected final sample after burst");
    assert!(
        inlet.samples_lost().unwrap_or(0) > 100,
        "expected substantial loss"
    );
    let true_last = t0 + last_idx as f64 * dt;
    let err = (last.timestamp - true_last).abs();
    assert!(
        err < dt,
        "expected dejitter error < {dt} s after live loss, got {err} s (ts={}, want={true_last})",
        last.timestamp
    );
}
