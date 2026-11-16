//! The outlet side of the `resume/1` feature, measured against a live
//! outlet. `docs/extensions.md`, section `resume`.
//!
//! These tests open real sockets on the loopback interface, so they belong to
//! the reporting tier and not to the merge gate. A failure here can come from
//! the machine.
//!
//! The first tests open the feed by hand and read the bytes as an inlet
//! does. The dropout tests at the end use a real inlet behind a proxy that
//! can cut the connection.

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_proto::features::{self, ResumeParam, Token};
use labstream_proto::feed::{FeedClient, FeedParams, TestPatternError};
use labstream_proto::seq::{self, Next};
use labstream_wire::{Codec, Format, Sample, TimestampDeducer, Value, DEDUCED_TIMESTAMP};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RATE: f64 = 1000.0;

fn outlet(name: &str) -> Option<Outlet> {
    let mut info = StreamInfo::new(name, "ResumeTest", 1, Format::Float32, RATE);
    info.source_id = format!("{name}_src");
    Outlet::new(info).ok()
}

/// Push the samples `from..to` in chunks of `len`, each chunk with one
/// timestamp. SPEC.md 9.2.
fn push(out: &Outlet, from: usize, to: usize, len: usize) {
    let mut k = from;
    while k < to {
        let end = (k + len).min(to);
        let block: Vec<Sample> = (k..end)
            .map(|i| Sample {
                timestamp: 0.0,
                values: vec![Value::F32(i as f32)],
            })
            .collect();
        out.push_chunk(&block, 500.0 + end as f64 / RATE + 0.000_123);
        k = end;
    }
}

/// One feed connection, read as an inlet reads it.
struct Feed {
    stream: TcpStream,
    codec: Codec,
    buf: Vec<u8>,
    deducer: TimestampDeducer,
    answer: Vec<Token>,
}

/// A received sample: its value, its wire timestamp, and its full timestamp.
type Got = (usize, f64, u64);

impl Feed {
    fn open(out: &Outlet, offer: Vec<Token>) -> Feed {
        let info = out.info();
        let mut stream =
            TcpStream::connect(("127.0.0.1", info.v4data_port)).expect("the data port answers");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let params = FeedParams::new(&info.uid, info.format);
        let (mut client, request) = FeedClient::start_with(params, info.format, 1, offer);
        stream.write_all(&request).unwrap();

        let mut buf = Vec::new();
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            match client.on_headers(&buf) {
                Ok(used) => {
                    buf.drain(..used);
                    break;
                }
                Err(labstream_proto::header::Error::Incomplete) => {}
                Err(e) => panic!("{e}"),
            }
            assert!(Instant::now() < end, "no answer headers");
            read_more(&mut stream, &mut buf);
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
            assert!(Instant::now() < end, "no test pattern");
            read_more(&mut stream, &mut buf);
        }
        Feed {
            stream,
            codec: *client.codec().expect("a codec"),
            buf,
            deducer: TimestampDeducer::new(RATE),
            answer: client.features().to_vec(),
        }
    }

    /// Read until `count` samples arrive. The result holds the records and
    /// the samples.
    fn read(&mut self, count: usize) -> (Vec<u64>, Vec<Got>) {
        let (mut records, mut samples) = (Vec::new(), Vec::new());
        let end = Instant::now() + Duration::from_secs(5);
        while samples.len() < count {
            match seq::peek(&self.buf) {
                Next::Record(n) => {
                    records.push(n);
                    self.buf.drain(..seq::RECORD_LEN);
                    continue;
                }
                Next::Incomplete => {}
                Next::Sample if !self.buf.is_empty() => {
                    if let Ok((s, used)) = self.codec.decode(&self.buf) {
                        self.buf.drain(..used);
                        let v = match s.values[0] {
                            Value::F32(v) => v as usize,
                            _ => panic!("wrong format"),
                        };
                        let full = self.deducer.apply(s.timestamp).to_bits();
                        samples.push((v, s.timestamp, full));
                        continue;
                    }
                }
                Next::Sample => {}
            }
            assert!(
                Instant::now() < end,
                "only {} of {count} samples",
                samples.len()
            );
            read_more(&mut self.stream, &mut self.buf);
        }
        (records, samples)
    }
}

fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>) {
    let mut tmp = [0u8; 65536];
    match stream.read(&mut tmp) {
        Ok(0) => panic!("the outlet closed the connection"),
        Ok(n) => buf.extend_from_slice(&tmp[..n]),
        Err(_) => {}
    }
}

fn offer(param: Option<ResumeParam>) -> Vec<Token> {
    let allowed = vec![features::SEQ.to_string(), features::RESUME.to_string()];
    let mut tokens = features::offer(&allowed);
    tokens.extend(features::offer_resume(&allowed, param));
    tokens
}

fn values(samples: &[Got]) -> Vec<usize> {
    samples.iter().map(|s| s.0).collect()
}

#[test]
fn a_second_connection_gets_the_samples_that_the_first_one_missed() {
    let out = match outlet("ResumeLive1") {
        Some(o) => o,
        None => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let mut a = Feed::open(&out, offer(None));
    let given = features::resume_answer(&a.answer).expect("the outlet answers resume/1");
    assert_eq!(given.number, 0);
    // Chunks of 7 put the cut at 50 in the middle of a chunk.
    push(&out, 0, 100, 7);
    let (records, first) = a.read(100);
    assert_eq!(records, vec![0]);
    assert_eq!(values(&first), (0..100).collect::<Vec<_>>());
    drop(a);

    // The outlet pushes with no consumer, then the inlet comes back. It
    // claims that it received up to sample 49 only.
    push(&out, 100, 200, 7);
    let mut b = Feed::open(
        &out,
        offer(Some(ResumeParam {
            epoch: given.epoch,
            number: 50,
        })),
    );
    let again = features::resume_answer(&b.answer).expect("the outlet answers resume/1");
    assert_eq!(again.epoch, given.epoch, "the same run of the outlet");
    assert_eq!(again.number, 200, "the first live sample");
    push(&out, 200, 220, 7);
    let (records, second) = b.read(170);
    assert_eq!(records, vec![50], "one record, and no gap");
    assert_eq!(values(&second), (50..220).collect::<Vec<_>>());

    for s in &second[..150] {
        assert_ne!(s.1, DEDUCED_TIMESTAMP, "resent sample {}", s.0);
    }
    // A resent sample has the timestamp that the first connection gave it,
    // bit for bit.
    for (old, new) in first[50..].iter().zip(&second) {
        assert_eq!(old.0, new.0);
        assert_eq!(old.2, new.2, "sample {}", old.0);
    }
}

#[test]
fn a_different_epoch_gets_no_resend() {
    let out = match outlet("ResumeLive2") {
        Some(o) => o,
        None => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let a = Feed::open(&out, offer(None));
    let given = features::resume_answer(&a.answer).expect("the outlet answers resume/1");
    push(&out, 0, 30, 10);
    drop(a);

    let mut b = Feed::open(
        &out,
        offer(Some(ResumeParam {
            epoch: given.epoch ^ 1,
            number: 0,
        })),
    );
    let again = features::resume_answer(&b.answer).expect("the outlet answers resume/1");
    assert_eq!(again.epoch, given.epoch, "the outlet gives its own epoch");
    assert_eq!(again.number, 30);
    push(&out, 30, 35, 10);
    let (records, got) = b.read(5);
    assert_eq!(records, vec![30]);
    assert_eq!(values(&got), (30..35).collect::<Vec<_>>());
}

#[test]
fn an_offer_without_seq_gets_no_resume() {
    let out = match outlet("ResumeLive3") {
        Some(o) => o,
        None => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let a = Feed::open(&out, vec![Token::new(features::RESUME, 1)]);
    assert!(a.answer.is_empty());
    let b = Feed::open(
        &out,
        vec![
            Token::new(features::SEQ, 1),
            Token::new(features::RESUME, 1),
            Token::new(features::RESUME, 1),
        ],
    );
    assert!(features::has(&b.answer, features::SEQ, 1));
    assert_eq!(features::resume_answer(&b.answer), None, "two tokens");
}

#[test]
fn a_blocking_outlet_gets_no_resume() {
    // A blocking outlet has no queue and no history. `docs/extensions.md`,
    // negotiation rule 11.
    let mut info = StreamInfo::new("ResumeLive4", "ResumeTest", 1, Format::Float32, RATE);
    info.source_id = "ResumeLive4_src".to_string();
    let out = match Outlet::new_blocking(info) {
        Ok(o) => o,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let a = Feed::open(&out, offer(None));
    assert!(features::has(&a.answer, features::SEQ, 1));
    assert_eq!(features::resume_answer(&a.answer), None);
}

/// A TCP proxy that forwards each connection to `target`, and can cut them.
struct Proxy {
    port: u16,
    accepted: Arc<Mutex<Vec<TcpStream>>>,
    /// While true, the proxy closes each new connection at once.
    paused: Arc<AtomicBool>,
}

impl Proxy {
    fn start(target: u16) -> Option<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let list = Arc::clone(&accepted);
        let paused = Arc::new(AtomicBool::new(false));
        let hold = Arc::clone(&paused);
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                if hold.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let server = match TcpStream::connect(("127.0.0.1", target)) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                list.lock().unwrap().push(client.try_clone().unwrap());
                pipe(client.try_clone().unwrap(), server.try_clone().unwrap());
                pipe(server, client);
            }
        });
        Some(Proxy {
            port,
            accepted,
            paused,
        })
    }

    /// Cut each connection, and refuse new ones for `time`.
    fn cut_for(&self, time: Duration) {
        self.paused.store(true, Ordering::SeqCst);
        for s in self.accepted.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
        std::thread::sleep(time);
        self.paused.store(false, Ordering::SeqCst);
    }
}

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while let Ok(n) = from.read(&mut buf) {
            if n == 0 || to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
}

/// The value of a sample of a dropout test: channel 0 holds the index.
fn index(s: &Sample) -> usize {
    match &s.values[0] {
        Value::F32(v) => *v as usize,
        Value::Str(b) => String::from_utf8_lossy(b).parse().unwrap(),
        _ => panic!("wrong format"),
    }
}

/// Pull until a sample with the index `last` arrives, or the time runs out.
fn pull_until(inlet: &mut Inlet, got: &mut Vec<Sample>, last: usize, within: Duration) {
    let end = Instant::now() + within;
    while Instant::now() < end && got.last().map(index) != Some(last) {
        let _ = inlet.pull_chunk(got, 1_000_000, Duration::from_millis(20));
    }
}

/// One dropout test. The outlet pushes chunks of `chunk` samples at `rate`,
/// and the proxy cuts the connection for 3 s. The inlet behind the proxy
/// must get each sample once, in order, with the same values and
/// timestamps as an inlet that connects directly. `docs/extensions.md`,
/// conformance item 9.
fn dropout(name: &str, format: Format, channels: usize, rate: f64, chunk: usize) {
    let mut info = StreamInfo::new(name, "ResumeTest", channels as u32, format, rate);
    info.source_id = format!("{name}_src");
    let out = match Outlet::new(info) {
        Ok(o) => Arc::new(o),
        Err(_) => {
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
    let mut cut = Inlet::open_recovering(&via_proxy, Duration::from_secs(5), 1_000_000)
        .expect("the inlet connects through the proxy");
    let mut direct = Inlet::open_recovering(&real, Duration::from_secs(5), 1_000_000)
        .expect("the inlet connects directly");
    let start = Instant::now();
    while out.consumer_count() < 2 && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        cut.samples_resent(),
        Some(0),
        "the connection uses resume/1"
    );

    // The source pushes one chunk each `chunk / rate` seconds. A rate of 0
    // is a marker stream, with one marker each 5 ms.
    let pushed = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let source = {
        let (out, pushed, stop) = (Arc::clone(&out), Arc::clone(&pushed), Arc::clone(&stop));
        std::thread::spawn(move || {
            let period = if rate > 0.0 {
                Duration::from_secs_f64(chunk as f64 / rate)
            } else {
                Duration::from_millis(5)
            };
            let mut k = 0;
            while !stop.load(Ordering::SeqCst) {
                let block: Vec<Sample> = (k..k + chunk)
                    .map(|i| Sample {
                        timestamp: 0.0,
                        values: (0..channels)
                            .map(|c| match format {
                                Format::String => Value::Str(
                                    if c == 0 {
                                        i.to_string()
                                    } else {
                                        format!("m{i}")
                                    }
                                    .into_bytes(),
                                ),
                                _ => Value::F32(if c == 0 { i as f32 } else { c as f32 }),
                            })
                            .collect(),
                    })
                    .collect();
                out.push_chunk(&block, 0.0);
                k += chunk;
                pushed.store(k, Ordering::SeqCst);
                std::thread::sleep(period);
            }
        })
    };

    let mut got = Vec::new();
    let mut reference = Vec::new();
    std::thread::sleep(Duration::from_millis(500));
    let _ = cut.pull_chunk(&mut got, 1_000_000, Duration::from_millis(20));
    assert!(!got.is_empty(), "samples arrive before the cut");
    proxy.cut_for(Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::SeqCst);
    source.join().unwrap();
    let last = pushed.load(Ordering::SeqCst) - 1;
    pull_until(&mut cut, &mut got, last, Duration::from_secs(15));
    pull_until(&mut direct, &mut reference, last, Duration::from_secs(5));

    assert!(cut.recoveries() >= 1, "the inlet connected again");
    let first = index(&got[0]);
    let indices: Vec<usize> = got.iter().map(index).collect();
    assert_eq!(
        indices,
        (first..=last).collect::<Vec<_>>(),
        "each sample once, in order"
    );
    assert_eq!(cut.samples_lost(), Some(0));
    assert_eq!(cut.samples_dropped(), 0);
    let resent = cut.samples_resent().expect("the connection uses resume/1");
    // The cut lasts 3 s. At least 2 s of samples must come from the
    // history. A marker stream pushes 200 markers each second.
    let at_least = 2.0 * if rate > 0.0 { rate } else { 200.0 };
    assert!(
        resent as f64 >= at_least,
        "the history gave the samples of the dropout: {resent}"
    );
    println!(
        "{name}: {} samples, {resent} resent, {} recoveries",
        got.len(),
        cut.recoveries()
    );

    // The same values and timestamps as the direct inlet, bit for bit.
    let offset = first - index(&reference[0]);
    for (a, b) in got.iter().zip(&reference[offset..]) {
        assert_eq!(index(a), index(b));
        assert_eq!(a.values, b.values, "sample {}", index(a));
        assert_eq!(
            a.timestamp.to_bits(),
            b.timestamp.to_bits(),
            "sample {}",
            index(a)
        );
    }
}

#[test]
fn a_dropout_of_3_s_at_512_hz_loses_nothing() {
    dropout("ResumeDrop1", Format::Float32, 8, 512.0, 8);
}

#[test]
fn a_dropout_of_3_s_at_4000_hz_and_64_channels_loses_nothing() {
    dropout("ResumeDrop2", Format::Float32, 64, 4000.0, 40);
}

#[test]
fn a_dropout_of_3_s_on_a_marker_stream_loses_nothing() {
    dropout("ResumeDrop3", Format::String, 2, 0.0, 1);
}
