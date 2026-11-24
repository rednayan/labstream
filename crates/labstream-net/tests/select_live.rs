//! Live integration tests for `select/1` (channel and rate selection,
//! `docs/roadmap.md`, step 6).

use labstream_net::{Inlet, Outlet, SelectParam, StreamInfo};
use labstream_proto::features;
use labstream_proto::feed::{FeedClient, FeedParams};
use labstream_proto::select::StreamSelector;
use labstream_wire::{ByteOrder, Codec, Format, PackedSample, TEST_PATTERN_OFFSETS};
use std::f64::consts::PI;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

fn info_with_labels(name: &str, ch: u32, srate: f64, fmt: Format) -> StreamInfo {
    let mut info = StreamInfo::new(name, "EEG", ch, fmt, srate);
    info.source_id = format!("{name}-src");
    let channels = info.desc.append_child("channels");
    for i in 0..ch {
        let c = channels.append_child("channel");
        c.append_child_value("label", &format!("Ch{i}"));
        c.append_child_value("unit", "uV");
    }
    info
}

fn channel_labels(info: &StreamInfo) -> Vec<String> {
    let Some(channels) = info.desc.child("channels") else {
        return Vec::new();
    };
    channels
        .children
        .iter()
        .filter(|c| c.name == "channel")
        .map(|c| c.child_value_of("label").to_string())
        .collect()
}

/// Criterion 1 & 3:
/// - Selected channel values keep the exact same bits as the full stream
///   (including `-0.0` and NaN payloads).
/// - A full consumer on the same outlet stays unaffected.
/// - Link bytes decrease in proportion to the channel ratio.
/// - `info()` and `fullinfo()` report the selected channel count and labels.
#[test]
fn select_channels_keeps_exact_bits_updates_desc_and_reduces_link_bytes() {
    let outlet = match Outlet::new(info_with_labels("SelCh64", 64, 500.0, Format::Float32)) {
        Ok(o) => o,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let pub_info = outlet.info().clone();

    let wanted = vec![3u16, 0, 63, 17];
    let mut full_inlet =
        Inlet::open_with_buffer(&pub_info, Duration::from_secs(5), 1024).expect("full inlet");
    let mut sel_inlet = Inlet::open_selected(
        &pub_info,
        Duration::from_secs(5),
        1024,
        false,
        SelectParam::channels(wanted.clone()),
    )
    .expect("selected inlet");

    assert_eq!(
        sel_inlet.selection(),
        Some(SelectParam::channels(wanted.clone()))
    );
    assert_eq!(sel_inlet.info().channel_count, 4);
    assert_eq!(sel_inlet.info().nominal_srate, 500.0);

    let full_desc = sel_inlet
        .fullinfo(Duration::from_secs(5))
        .expect("selected fullinfo");
    assert_eq!(full_desc.channel_count, 4);
    assert_eq!(full_desc.nominal_srate, 500.0);
    assert_eq!(
        channel_labels(&full_desc),
        vec!["Ch3", "Ch0", "Ch63", "Ch17"]
    );

    // Wait for both consumers to register.
    let wait_end = Instant::now() + Duration::from_secs(5);
    while outlet.consumer_count() < 2 && Instant::now() < wait_end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(outlet.consumer_count(), 2);

    let neg_zero = -0.0f32;
    let nan_payload = f32::from_bits(0x7fc0_1234);
    let n_samples = 80usize;
    for k in 0..n_samples {
        let mut row: Vec<f32> = (0..64).map(|c| (k * 100 + c) as f32 + 0.25).collect();
        if k == 5 {
            row[3] = neg_zero;
            row[63] = nan_payload;
        }
        outlet.push_packed(PackedSample::from_f32(1000.0 + k as f64 / 500.0, &row));
    }

    let mut full_got = Vec::new();
    let mut sel_got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (full_got.len() < n_samples || sel_got.len() < n_samples) && Instant::now() < deadline {
        let _ = full_inlet.pull_chunk_packed(&mut full_got, n_samples, Duration::from_millis(50));
        let _ = sel_inlet.pull_chunk_packed(&mut sel_got, n_samples, Duration::from_millis(50));
    }
    assert_eq!(full_got.len(), n_samples);
    assert_eq!(sel_got.len(), n_samples);

    for k in 0..n_samples {
        assert_eq!(full_got[k].timestamp, sel_got[k].timestamp);
        let mut fv = Vec::new();
        let mut sv = Vec::new();
        full_got[k].push_into_f32(&mut fv);
        sel_got[k].push_into_f32(&mut sv);
        assert_eq!(fv.len(), 64);
        assert_eq!(sv.len(), 4);
        for (out_i, &src_ch) in wanted.iter().enumerate() {
            assert_eq!(
                sv[out_i].to_bits(),
                fv[src_ch as usize].to_bits(),
                "bit mismatch at sample {k} channel {src_ch}"
            );
        }
    }
    assert_eq!(sel_inlet.samples_lost(), Some(0));

    // Measure raw TCP payload bytes for 100 samples: full (64 ch) vs selected (4 ch).
    drop(full_inlet);
    drop(sel_inlet);
    let full_bytes = count_raw_tcp_data_bytes(&pub_info, &outlet, None, 100);
    let sel_bytes =
        count_raw_tcp_data_bytes(&pub_info, &outlet, Some(SelectParam::channels(wanted)), 100);
    // 64 ch f32 = 265 B/sample (~26,500 B), 4 ch f32 = 25 B/sample (~2,500 B) -> > 8x reduction.
    assert!(
        sel_bytes * 8 < full_bytes,
        "expected > 8x wire byte reduction: full={full_bytes}, selected={sel_bytes}"
    );
}

/// Connect directly over TCP (offering only `seq/1` + optional `select/1`, no
/// `dgram/1`) so every sample byte crosses the TCP socket, push `n` samples,
/// and return the exact wire byte count received after the test pattern.
fn count_raw_tcp_data_bytes(
    info: &StreamInfo,
    outlet: &Outlet,
    sel: Option<SelectParam>,
    n: usize,
) -> usize {
    let addr = format!("127.0.0.1:{}", info.v4data_port);
    let mut stream = TcpStream::connect(&addr).expect("raw tcp connect");
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut params = FeedParams::new(&info.uid, info.format);
    params.max_buffer_length = 2048;
    let mut offer = vec![features::Token::new(features::SEQ, 1)];
    if let Some(s) = &sel {
        offer.push(s.token());
    }
    let (mut client, req) =
        FeedClient::start_with(params, info.format, info.channel_count as usize, offer);
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
                let k = stream.read(&mut tmp).unwrap();
                buf.extend_from_slice(&tmp[..k]);
            }
            Err(e) => panic!("headers error: {e}"),
        }
    }
    loop {
        match client.on_test_pattern(&buf) {
            Ok(used) => {
                buf.drain(..used);
                break;
            }
            Err(labstream_proto::feed::TestPatternError::Incomplete) => {
                let k = stream.read(&mut tmp).unwrap();
                buf.extend_from_slice(&tmp[..k]);
            }
            Err(e) => panic!("test pattern error: {e:?}"),
        }
    }

    let out_ch = sel.as_ref().map_or(info.channel_count as usize, |s| {
        s.output_channels(info.channel_count as usize)
    });
    let codec = Codec::new(info.format, out_ch, ByteOrder::native());
    let mut wire_bytes = buf.len();
    let mut decoded = 0usize;

    // Wait for consumer registration, then push `n` samples.
    let wait_end = Instant::now() + Duration::from_secs(2);
    while outlet.consumer_count() == 0 && Instant::now() < wait_end {
        std::thread::sleep(Duration::from_millis(5));
    }
    let row = vec![1.25f32; info.channel_count as usize];
    for k in 0..n {
        outlet.push_packed(PackedSample::from_f32(500.0 + k as f64 / 500.0, &row));
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while decoded < n && Instant::now() < deadline {
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
                Ok((_, used)) => {
                    pos += used;
                    decoded += 1;
                }
                Err(labstream_wire::Error::Truncated) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        buf.drain(..pos);
        if decoded >= n {
            break;
        }
        let k = stream.read(&mut tmp).unwrap();
        assert!(k > 0, "unexpected EOF after {decoded} samples");
        wire_bytes += k;
        buf.extend_from_slice(&tmp[..k]);
    }
    assert_eq!(decoded, n);
    wire_bytes
}

/// Criterion 2 & 3:
/// - Decimation by 16 (4000 Hz -> 250 Hz) + channel subset `[5, 2]` matches
///   an independent reference `StreamSelector` sample-for-sample and timestamp-for-timestamp.
/// - `info()` and `fullinfo()` report `250.0` Hz and the selected channel labels.
#[test]
fn select_decimation_matches_reference_fir_and_updates_fullinfo() {
    let outlet = match Outlet::new(info_with_labels("SelDecim", 8, 4000.0, Format::Double64)) {
        Ok(o) => o,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let pub_info = outlet.info().clone();

    let sel = SelectParam {
        decimate: 16,
        channels: vec![5, 2],
    };
    let mut inlet =
        Inlet::open_selected(&pub_info, Duration::from_secs(5), 1024, false, sel.clone())
            .expect("decimated inlet");

    assert_eq!(inlet.selection(), Some(sel.clone()));
    assert_eq!(inlet.info().channel_count, 2);
    assert_eq!(inlet.info().nominal_srate, 250.0);

    let full = inlet.fullinfo(Duration::from_secs(5)).expect("fullinfo");
    assert_eq!(full.channel_count, 2);
    assert_eq!(full.nominal_srate, 250.0);
    assert_eq!(channel_labels(&full), vec!["Ch5", "Ch2"]);

    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));

    let mut reference = StreamSelector::new(sel, Format::Double64, 8);
    let mut expected: Vec<(f64, Vec<f64>)> = Vec::new();
    let n_in = 800usize;
    for i in 0..n_in {
        let ts = 10.0 + i as f64 / 4000.0;
        let mut row = [0.0f64; 8];
        for (c, slot) in row.iter_mut().enumerate() {
            *slot = (2.0 * PI * 20.0 * ts).sin()
                + 0.4 * (2.0 * PI * 300.0 * ts + c as f64).cos()
                + c as f64;
        }
        let sample = PackedSample::from_f64(ts, &row);
        if let Some(emitted) = reference.push(&sample, ts) {
            let mut vals = Vec::new();
            emitted.push_into_f64(&mut vals);
            expected.push((emitted.timestamp, vals));
        }
        outlet.push_packed(sample);
    }

    assert!(!expected.is_empty());
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.len() < expected.len() && Instant::now() < deadline {
        let _ = inlet.pull_chunk_packed(&mut got, expected.len(), Duration::from_millis(50));
    }
    assert_eq!(got.len(), expected.len());
    for (k, (g, (et, ev))) in got.iter().zip(&expected).enumerate() {
        assert!((g.timestamp - et).abs() < 1e-12, "ts mismatch at {k}");
        let mut gv = Vec::new();
        g.push_into_f64(&mut gv);
        assert_eq!(gv.len(), 2);
        for ch in 0..2 {
            assert!(
                (gv[ch] - ev[ch]).abs() < 1e-12,
                "value mismatch at sample {k} ch {ch}: {} vs {}",
                gv[ch],
                ev[ch]
            );
        }
    }
    assert_eq!(inlet.samples_lost(), Some(0));
}

/// When an outlet does not answer `select/1` (like `liblsl`), the inlet falls
/// back cleanly to the full stream without error.
#[test]
fn select_falls_back_cleanly_on_plain_lsl_outlet() {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let port = listener.local_addr().unwrap().port();
    let mut info = StreamInfo::new("PlainLsl", "EEG", 4, Format::Float32, 100.0);
    info.uid = "plain-lsl-select-fallback".to_string();
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
        // Plain LSL 1.10 response with NO Labstream-Features header.
        let reply = format!(
            "LSL/110 200 OK\r\n\
             UID: {uid}\r\n\
             Byte-Order: 1234\r\n\
             Suppress-Subnormals: 0\r\n\
             Data-Protocol-Version: 110\r\n\r\n"
        );
        s.write_all(reply.as_bytes()).unwrap();
        let codec = Codec::new(Format::Float32, 4, ByteOrder::Little);
        let mut out = Vec::new();
        for off in TEST_PATTERN_OFFSETS {
            let tp = labstream_wire::test_pattern(Format::Float32, 4, off);
            codec.encode(&tp, &mut out).unwrap();
        }
        for k in 0..5 {
            let sample = PackedSample::from_f32(
                50.0 + k as f64,
                &[k as f32, k as f32 + 1.0, k as f32 + 2.0, k as f32 + 3.0],
            );
            codec.encode_packed(&sample, &mut out).unwrap();
        }
        s.write_all(&out).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    });

    let mut inlet = Inlet::open_selected(
        &info,
        Duration::from_secs(5),
        64,
        false,
        SelectParam {
            decimate: 4,
            channels: vec![2, 0],
        },
    )
    .expect("plain fallback inlet");

    assert_eq!(inlet.selection(), None);
    assert_eq!(inlet.info().channel_count, 4);
    assert_eq!(inlet.info().nominal_srate, 100.0);

    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while got.len() < 5 && Instant::now() < deadline {
        let _ = inlet.pull_chunk_packed(&mut got, 5, Duration::from_millis(50));
    }
    assert_eq!(got.len(), 5);
    let mut vals = Vec::new();
    got[3].push_into_f32(&mut vals);
    assert_eq!(vals, vec![3.0, 4.0, 5.0, 6.0]);
    let _ = server.join();
}
