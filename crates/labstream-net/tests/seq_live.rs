//! The `seq/1` feature, measured against a live outlet. `docs/extensions.md`.
//!
//! These tests open real sockets on the loopback interface, so they belong to
//! the reporting tier and not to the merge gate. A failure here can come from
//! the machine.
//!
//! Each test makes sure that the counts add up: every pushed sample is
//! received, counted as lost, or counted as dropped by the inlet queue.

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_proto::features::{self, Token};
use labstream_proto::feed::{FeedClient, FeedParams, TestPatternError};
use labstream_proto::seq;
use labstream_wire::{Format, Sample, Value};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn sample(k: usize) -> Sample {
    Sample {
        timestamp: 0.0,
        values: vec![Value::F32(k as f32)],
    }
}

fn value(s: &Sample) -> usize {
    match s.values[0] {
        Value::F32(v) => v as usize,
        _ => panic!("wrong format"),
    }
}

fn outlet(name: &str) -> Option<Outlet> {
    let mut info = StreamInfo::new(name, "SeqTest", 1, Format::Float32, 1000.0);
    info.source_id = format!("{name}_src");
    Outlet::new(info).ok()
}

/// Pull until a sample with the value `last` arrives, or the time runs out.
fn pull_until(inlet: &mut Inlet, last: usize, within: Duration) -> Vec<Sample> {
    let mut got = Vec::new();
    let end = Instant::now() + within;
    while Instant::now() < end {
        let mut block = Vec::new();
        let _ = inlet.pull_chunk(&mut block, 100_000, Duration::from_millis(20));
        let done = block.iter().any(|s| value(s) == last);
        got.extend(block);
        if done {
            break;
        }
    }
    got
}

#[test]
fn the_count_adds_up_when_the_outlet_queue_drops() {
    let out = match outlet("SeqLive1") {
        Some(o) => o,
        None => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    // The inlet asks for 32 samples of buffer, so the outlet keeps a queue
    // of 32 for it.
    let mut inlet =
        Inlet::open_recovering(out.info(), Duration::from_secs(5), 32).expect("the inlet opens");
    assert!(out.wait_for_consumers(Duration::from_secs(5)));

    // One sample sets the baseline of the count.
    out.push(&sample(0));
    let first = pull_until(&mut inlet, 0, Duration::from_secs(5));
    assert_eq!(first.len(), 1, "the first sample arrives");
    assert_eq!(inlet.samples_lost(), Some(0), "two labstream peers use seq");

    // One push of a large block. The queue of 32 keeps the last 32 samples.
    let n = 100_000;
    out.push_samples((1..=n).map(sample).collect());
    let got = pull_until(&mut inlet, n, Duration::from_secs(10));
    assert_eq!(got.last().map(value), Some(n), "the last sample arrives");

    let lost = inlet.samples_lost().expect("a count");
    let dropped = inlet.samples_dropped();
    println!("received {}, lost {lost}, dropped {dropped}", got.len());
    assert_eq!(got.len() as u64 + lost + dropped, n as u64);
    assert!(
        lost >= (n - 64) as u64,
        "the outlet dropped most of the block"
    );
}

/// A TCP proxy that forwards each connection to `target`, and can cut them.
struct Proxy {
    port: u16,
    accepted: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn start(target: u16) -> Option<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let list = Arc::clone(&accepted);
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let server = match TcpStream::connect(("127.0.0.1", target)) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                list.lock().unwrap().push(client.try_clone().unwrap());
                pipe(client.try_clone().unwrap(), server.try_clone().unwrap());
                pipe(server, client);
            }
        });
        Some(Proxy { port, accepted })
    }

    fn cut(&self) {
        for s in self.accepted.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        while let Ok(n) = from.read(&mut buf) {
            if n == 0 || to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
}

#[test]
fn the_count_shows_the_samples_of_a_lost_connection() {
    let out = match outlet("SeqLive2") {
        Some(o) => Arc::new(o),
        None => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let real = out.info().clone();
    let proxy = match Proxy::start(real.v4data_port) {
        Some(p) => p,
        None => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let mut via_proxy = real.clone();
    via_proxy.v4address = "127.0.0.1".to_string();
    via_proxy.v4data_port = proxy.port;
    let mut inlet = Inlet::open_recovering(&via_proxy, Duration::from_secs(5), 100_000)
        .expect("the inlet connects through the proxy");
    assert!(out.wait_for_consumers(Duration::from_secs(5)));

    // A source thread pushes one sample each 200 us.
    let pushed = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let source = {
        let (out, pushed, stop) = (Arc::clone(&out), Arc::clone(&pushed), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut k = 0;
            while !stop.load(Ordering::SeqCst) {
                out.push(&sample(k));
                k += 1;
                pushed.store(k, Ordering::SeqCst);
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };

    let mut got = pull_until(&mut inlet, 50, Duration::from_secs(5));
    assert!(!got.is_empty(), "samples arrive before the cut");
    proxy.cut();
    // Wait for samples from after the recovery.
    let cut_at = pushed.load(Ordering::SeqCst);
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && got.last().map(value).unwrap_or(0) < cut_at + 200 {
        let mut block = Vec::new();
        let _ = inlet.pull_chunk(&mut block, 100_000, Duration::from_millis(20));
        got.extend(block);
    }
    stop.store(true, Ordering::SeqCst);
    source.join().unwrap();
    let last = pushed.load(Ordering::SeqCst) - 1;
    if got.last().map(value) != Some(last) {
        got.extend(pull_until(&mut inlet, last, Duration::from_secs(5)));
    }
    assert_eq!(got.last().map(value), Some(last), "the last sample arrives");

    assert!(inlet.recoveries() >= 1, "the inlet connected again");
    let first = value(&got[0]);
    let lost = inlet.samples_lost().expect("a count after the recovery");
    let dropped = inlet.samples_dropped();
    let expected = (last - first + 1) as u64;
    println!(
        "pushed {expected} after the first received, received {}, lost {lost}, dropped {dropped}",
        got.len()
    );
    assert_eq!(got.len() as u64 + lost + dropped, expected);
}

/// Open a feed by hand, and give the answer features and the first byte after
/// the test pattern.
fn raw_feed(out: &Outlet, offer: Vec<Token>) -> (Vec<Token>, u8) {
    let info = out.info();
    let mut stream =
        TcpStream::connect(("127.0.0.1", info.v4data_port)).expect("the data port answers");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let params = FeedParams::new(&info.uid, info.format);
    let (mut client, request) = FeedClient::start_with(params, info.format, 1, offer);
    stream.write_all(&request).unwrap();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match client.on_headers(&buf) {
            Ok(used) => {
                buf.drain(..used);
                break;
            }
            Err(labstream_proto::header::Error::Incomplete) => {}
            Err(e) => panic!("{e}"),
        }
        let n = stream.read(&mut tmp).unwrap();
        assert!(n > 0, "the outlet closed in the headers");
        buf.extend_from_slice(&tmp[..n]);
    }
    loop {
        match client.on_test_pattern(&buf) {
            Ok(used) => {
                buf.drain(..used);
                break;
            }
            Err(TestPatternError::Incomplete) => {}
            Err(e) => panic!("{e:?}"),
        }
        let n = stream.read(&mut tmp).unwrap();
        buf.extend_from_slice(&tmp[..n]);
    }
    // The consumer exists now. One push gives the first data byte.
    let end = Instant::now() + Duration::from_secs(5);
    while buf.is_empty() && Instant::now() < end {
        out.push(&sample(1));
        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
        if let Ok(n) = stream.read(&mut tmp) {
            buf.extend_from_slice(&tmp[..n]);
        }
    }
    (client.features().to_vec(), buf[0])
}

#[test]
fn a_request_without_the_header_gets_plain_samples() {
    let out = match outlet("SeqLive3") {
        Some(o) => o,
        None => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let (agreed, first) = raw_feed(&out, Vec::new());
    assert!(agreed.is_empty());
    assert!(
        first == 1 || first == 2,
        "a sample tag, not a record: {first}"
    );

    let (agreed, first) = raw_feed(&out, vec![Token::new(features::SEQ, 1)]);
    assert!(features::has(&agreed, features::SEQ, 1));
    assert_eq!(first, seq::TAG, "a numbered feed starts with a record");
}
