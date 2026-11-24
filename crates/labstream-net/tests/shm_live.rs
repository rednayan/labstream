//! Live integration tests for `shm/1` shared-memory transport
//! (`docs/extensions.md`, section `shm`, `docs/roadmap.md`, step 9).

use labstream_net::{Inlet, Outlet, SelectParam, StreamInfo};
use labstream_proto::features;
use labstream_proto::feed::{FeedClient, FeedParams};
use labstream_wire::{ByteOrder, Codec, Format, PackedSample, TEST_PATTERN_OFFSETS};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// Verify same-machine `shm/1` negotiation, bit-exact delivery (including
/// `-0.0` and NaN payloads), zero loss, and zero leaked `/dev/shm/ls-shm-*`
/// entries once connected and dropped.
#[test]
fn shm_delivers_exact_bits_with_zero_loss_and_cleans_up_segments() {
    let mut info = StreamInfo::new("ShmLive64", "EEG", 64, Format::Float32, 4000.0);
    info.source_id = "shm-live-64".to_string();
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };

    let mut inlet =
        Inlet::open_with_buffer(outlet.info(), Duration::from_secs(5), 4096).expect("shm inlet");
    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));
    assert!(inlet.on_shm(), "same-machine numeric inlet must use shm/1");
    assert!(!inlet.on_udp(), "shm/1 must not also use UDP");
    assert!(inlet.health().on_shm);

    let nan = f32::from_bits(0x7fc0_9876);
    let neg_zero = -0.0f32;
    let total = 200usize;
    for k in 0..total {
        let mut row: Vec<f32> = (0..64).map(|c| (k * 64 + c) as f32).collect();
        if k == 7 {
            row[1] = neg_zero;
            row[63] = nan;
        }
        outlet.push_packed(PackedSample::from_f32(1000.0 + k as f64 / 4000.0, &row));
    }

    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.len() < total && Instant::now() < deadline {
        let _ = inlet.pull_chunk_packed(&mut got, total, Duration::from_millis(50));
    }
    assert_eq!(got.len(), total);
    assert_eq!(inlet.samples_lost(), Some(0));
    assert_eq!(inlet.samples_dropped(), 0);

    for (k, s) in got.iter().enumerate() {
        assert_eq!(s.timestamp, 1000.0 + k as f64 / 4000.0);
        let mut vals = Vec::new();
        s.push_into_f32(&mut vals);
        assert_eq!(vals.len(), 64);
        if k == 7 {
            assert_eq!(vals[1].to_bits(), neg_zero.to_bits());
            assert_eq!(vals[63].to_bits(), nan.to_bits());
        } else {
            assert_eq!(vals[0], (k * 64) as f32);
            assert_eq!(vals[63], (k * 64 + 63) as f32);
        }
    }

    // Channel subset selection (`select/1` with decimate=1) works over `shm/1` too.
    let mut sel_inlet = Inlet::open_selected(
        outlet.info(),
        Duration::from_secs(5),
        1024,
        false,
        SelectParam::channels(vec![63, 0]),
    )
    .expect("selected shm inlet");
    assert!(sel_inlet.on_shm());
    let wait = Instant::now() + Duration::from_secs(2);
    while outlet.consumer_count() < 2 && Instant::now() < wait {
        std::thread::sleep(Duration::from_millis(5));
    }
    let row: Vec<f32> = (0..64).map(|c| c as f32 + 0.5).collect();
    outlet.push_packed(PackedSample::from_f32(2000.0, &row));
    let s = sel_inlet
        .pull_packed(Duration::from_secs(2))
        .unwrap()
        .expect("projected shm sample");
    let mut sv = Vec::new();
    s.push_into_f32(&mut sv);
    assert_eq!(sv, vec![63.5, 0.5]);

    drop(sel_inlet);
    drop(inlet);
    drop(outlet);

    // On Linux, `shm_unlink` runs immediately on `shm-ok` (and on drop),
    // leaving zero `ls-shm-*` files behind in `/dev/shm`.
    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = std::fs::read_dir("/dev/shm") {
            let leftovers: Vec<_> = entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("ls-shm-"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "expected zero leftover /dev/shm/ls-shm-* files, found {leftovers:?}"
            );
        }
    }
}

/// Criterion 3 (outlet side): when a remote peer or container replies `shm-no`,
/// the outlet unlinks the SHM segment and streams cleanly over TCP with no error.
#[test]
fn outlet_falls_back_to_tcp_when_peer_sends_shm_no() {
    let mut info = StreamInfo::new("ShmFallbackOut", "EEG", 4, Format::Float32, 250.0);
    info.source_id = "shm-fallback-out".to_string();
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let pub_info = outlet.info().clone();
    let mut stream =
        TcpStream::connect(format!("127.0.0.1:{}", pub_info.v4data_port)).expect("tcp connect");
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut params = FeedParams::new(&pub_info.uid, pub_info.format);
    params.max_buffer_length = 512;
    let offer = vec![
        features::Token::new(features::SEQ, 1),
        features::Token::new(features::SHM, 1),
    ];
    let (mut client, req) = FeedClient::start_with(params, pub_info.format, 4, offer);
    stream.write_all(&req).unwrap();
    stream.flush().unwrap();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match client.on_headers(&buf) {
            Ok(used) => {
                buf.drain(..used);
                break;
            }
            Err(labstream_proto::header::Error::Incomplete) => {
                let n = stream.read(&mut tmp).unwrap();
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) => panic!("headers: {e}"),
        }
    }
    assert!(
        features::shm_answer(client.features()).is_some(),
        "outlet must answer shm/1"
    );
    loop {
        match client.on_test_pattern(&buf) {
            Ok(used) => {
                buf.drain(..used);
                break;
            }
            Err(labstream_proto::feed::TestPatternError::Incomplete) => {
                let n = stream.read(&mut tmp).unwrap();
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) => panic!("test pattern: {e:?}"),
        }
    }

    // Simulate a remote machine / container that cannot open the SHM segment.
    stream.write_all(b"shm-no\n").unwrap();
    stream.flush().unwrap();

    assert!(outlet.wait_for_consumers(Duration::from_secs(3)));
    for k in 0..10 {
        outlet.push_packed(PackedSample::from_f32(
            300.0 + k as f64,
            &[k as f32, 1.0, 2.0, 3.0],
        ));
    }

    let codec = *client.codec().unwrap();
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while got.len() < 10 && Instant::now() < deadline {
        let mut pos = 0;
        loop {
            match labstream_proto::seq::peek(&buf[pos..]) {
                labstream_proto::seq::Next::Record(_) => {
                    pos += labstream_proto::seq::RECORD_LEN;
                    continue;
                }
                labstream_proto::seq::Next::Incomplete => break,
                labstream_proto::seq::Next::Sample => {}
            }
            match codec.decode_packed(&buf[pos..]) {
                Ok((s, used)) => {
                    pos += used;
                    got.push(s);
                }
                Err(labstream_wire::Error::Truncated) => break,
                Err(e) => panic!("decode: {e}"),
            }
        }
        buf.drain(..pos);
        if got.len() < 10 {
            let n = stream.read(&mut tmp).unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&tmp[..n]);
        }
    }
    assert_eq!(got.len(), 10);
    let mut v = Vec::new();
    got[9].push_into_f32(&mut v);
    assert_eq!(v, vec![9.0, 1.0, 2.0, 3.0]);
}

/// Criterion 3 (inlet side): when an outlet advertises an unreachable SHM
/// segment ID (as happens across machines or containers), the inlet sends
/// `shm-no` and receives samples cleanly over TCP with no error.
#[test]
fn inlet_sends_shm_no_and_reads_tcp_when_segment_unreachable() {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let port = listener.local_addr().unwrap().port();
    let mut info = StreamInfo::new("ShmFallbackIn", "EEG", 2, Format::Float32, 100.0);
    info.uid = "shm-fallback-in-uid".to_string();
    info.v4address = "127.0.0.1".to_string();
    info.v4data_port = port;

    let uid = info.uid.clone();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let k = s.read(&mut tmp).unwrap();
            if k == 0 {
                return;
            }
            buf.extend_from_slice(&tmp[..k]);
        }
        // Advertise a nonexistent SHM segment ID (as if from another host).
        let reply = format!(
            "LSL/110 200 OK\r\n\
             UID: {uid}\r\n\
             Byte-Order: 1234\r\n\
             Suppress-Subnormals: 0\r\n\
             Data-Protocol-Version: 110\r\n\
             Labstream-Features: seq/1 shm/1=deadbeefcafebabe\r\n\r\n"
        );
        s.write_all(reply.as_bytes()).unwrap();
        let codec = Codec::new(Format::Float32, 2, ByteOrder::Little);
        let mut tp = Vec::new();
        for off in TEST_PATTERN_OFFSETS {
            let p = labstream_wire::test_pattern(Format::Float32, 2, off);
            codec.encode(&p, &mut tp).unwrap();
        }
        s.write_all(&tp).unwrap();
        s.flush().unwrap();

        // Read the inlet's `shm-no\n` line.
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while s.read(&mut b).unwrap_or(0) == 1 {
            line.push(b[0]);
            if b[0] == b'\n' {
                break;
            }
        }
        assert_eq!(String::from_utf8_lossy(&line), "shm-no\n");

        // Now stream 5 samples over TCP.
        let mut out = Vec::new();
        labstream_proto::seq::write_record(&mut out, 0);
        for k in 0..5 {
            let sample = PackedSample::from_f32(50.0 + k as f64, &[k as f32, 99.0]);
            codec.encode_packed(&sample, &mut out).unwrap();
        }
        s.write_all(&out).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    });

    let mut inlet = Inlet::open_with_buffer(&info, Duration::from_secs(5), 64).expect("inlet");
    assert!(!inlet.on_shm(), "must fall back off SHM");
    assert!(!inlet.on_udp(), "must use TCP");

    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while got.len() < 5 && Instant::now() < deadline {
        let _ = inlet.pull_chunk_packed(&mut got, 5, Duration::from_millis(50));
    }
    assert_eq!(got.len(), 5);
    assert_eq!(inlet.samples_lost(), Some(0));
    let mut vals = Vec::new();
    got[4].push_into_f32(&mut vals);
    assert_eq!(vals, vec![4.0, 99.0]);
    server.join().unwrap();
}
