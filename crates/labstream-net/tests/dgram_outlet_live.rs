//! The outlet side of the `dgram/1` feature, measured against a live outlet.
//! `docs/extensions.md`, section `dgram`.
//!
//! `dgram` is off by default, so this file is its own test process with
//! `labstream.Features = {seq, resume, dgram}`. Each test opens the feed by
//! hand, sends the hello from its own UDP socket, and reads both paths as an
//! inlet does.

mod resume_common;

use labstream_net::{Outlet, StreamInfo};
use labstream_proto::dgram::{self, Datagram, Path, Record};
use labstream_proto::features::{self, DgramParam};
use labstream_proto::feed::{FeedClient, FeedParams, TestPatternError};
use labstream_proto::seq::{self, Next};
use labstream_wire::{Codec, Format, Sample, Value, DEDUCED_TIMESTAMP};
use resume_common::{push, use_configuration};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::Once;
use std::time::{Duration, Instant};

static CONFIG: Once = Once::new();

fn setup() {
    CONFIG.call_once(|| {
        use_configuration(
            "dgram-outlet",
            "[labstream]\nFeatures = {seq, resume, dgram}\n",
        )
    });
}

fn outlet(name: &str, format: Format) -> Option<Outlet> {
    setup();
    let mut info = StreamInfo::new(name, "DgramTest", 1, format, 1000.0);
    info.source_id = format!("{name}_src");
    Outlet::new(info).ok()
}

/// What arrived on TCP after the test pattern.
#[derive(Debug, PartialEq)]
enum Tcp {
    Seq(u64),
    Path(Path, u64),
    Sample(usize),
}

/// One hand-made `dgram` connection.
struct Conn {
    stream: TcpStream,
    udp: UdpSocket,
    codec: Codec,
    buf: Vec<u8>,
    param: DgramParam,
}

impl Conn {
    fn open(out: &Outlet) -> Conn {
        let info = out.info();
        let mut stream = TcpStream::connect(("127.0.0.1", info.v4data_port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let allowed: Vec<String> = ["seq", "resume", "dgram"].map(String::from).to_vec();
        let mut offer = features::offer(&allowed);
        offer.extend(features::offer_resume(&allowed, None));
        offer.extend(features::offer_dgram(&allowed));
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
            assert!(Instant::now() < end);
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
            assert!(Instant::now() < end);
            read_more(&mut stream, &mut buf);
        }
        let param = features::dgram_answer(client.features()).expect("the outlet answers dgram/1");
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        Conn {
            stream,
            udp,
            codec: *client.codec().unwrap(),
            buf,
            param,
        }
    }

    fn hello(&self, cookie: u64) {
        self.udp
            .send_to(&dgram::hello(cookie), ("127.0.0.1", self.param.port))
            .unwrap();
    }

    /// Read TCP for `time`, and give what arrived.
    fn tcp(&mut self, time: Duration) -> Vec<Tcp> {
        let mut got = Vec::new();
        let end = Instant::now() + time;
        while Instant::now() < end {
            read_more(&mut self.stream, &mut self.buf);
            loop {
                if let Next::Record(n) = seq::peek(&self.buf) {
                    got.push(Tcp::Seq(n));
                    self.buf.drain(..seq::RECORD_LEN);
                    continue;
                }
                match dgram::peek(&self.buf) {
                    Record::Path { path, number } => {
                        got.push(Tcp::Path(path, number));
                        self.buf.drain(..dgram::PATH_LEN);
                        continue;
                    }
                    Record::Incomplete => break,
                    _ => {}
                }
                if self.buf.is_empty() {
                    break;
                }
                match self.codec.decode(&self.buf) {
                    Ok((s, used)) => {
                        self.buf.drain(..used);
                        got.push(Tcp::Sample(value(&s)));
                    }
                    Err(_) => break,
                }
            }
        }
        got
    }

    /// Read UDP for `time`. Give the numbers of each data datagram, and the
    /// `next` of each idle datagram.
    fn udp(&self, time: Duration) -> (Vec<Vec<u64>>, Vec<u64>) {
        let (mut data, mut idle) = (Vec::new(), Vec::new());
        let end = Instant::now() + time;
        let mut b = [0u8; 2048];
        while Instant::now() < end {
            let Ok(n) = self.udp.recv(&mut b) else {
                continue;
            };
            assert!(n <= dgram::MAX_DATAGRAM);
            match dgram::parse(&b[..n]).unwrap() {
                Datagram::Data {
                    cookie,
                    first,
                    count,
                    payload,
                } => {
                    assert_eq!(cookie, self.param.cookie);
                    let mut at = 0;
                    let mut numbers = Vec::new();
                    for k in 0..count as u64 {
                        let (s, used) = self.codec.decode(&payload[at..]).unwrap();
                        at += used;
                        assert_ne!(s.timestamp, DEDUCED_TIMESTAMP, "a full timestamp");
                        if let Value::F32(_) = s.values[0] {
                            assert_eq!(value(&s) as u64, first + k, "consecutive samples");
                        }
                        numbers.push(first + k);
                    }
                    assert_eq!(at, payload.len());
                    data.push(numbers);
                }
                Datagram::Idle { cookie, next } => {
                    assert_eq!(cookie, self.param.cookie);
                    idle.push(next);
                }
                Datagram::Hello { .. } => panic!("the outlet sends no hello"),
            }
        }
        (data, idle)
    }

    fn request(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
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

fn value(s: &Sample) -> usize {
    match &s.values[0] {
        Value::F32(v) => *v as usize,
        Value::Str(b) => b.len(),
        _ => panic!("wrong format"),
    }
}

/// Switch a new connection to UDP, and give the number of the first sample
/// on UDP.
fn switched(out: &Outlet) -> (Conn, u64) {
    let mut c = Conn::open(out);
    c.hello(c.param.cookie);
    let got = c.tcp(Duration::from_millis(300));
    let n = match got.as_slice() {
        [Tcp::Seq(a), Tcp::Path(Path::Udp, b)] => {
            assert_eq!(a, b);
            *b
        }
        other => panic!("a seq record, then a path record: {other:?}"),
    };
    (c, n)
}

#[test]
fn a_hello_moves_the_samples_to_udp_with_copies() {
    let Some(out) = outlet("DgramOut1", Format::Float32) else {
        eprintln!("no local network; skipping");
        return;
    };
    let (mut c, n) = switched(&out);
    assert_eq!(n, 0);
    for k in 0..100 {
        push(&out, k, k + 1, 1);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (data, idle) = c.udp(Duration::from_millis(1500));
    let mut seen = std::collections::HashMap::new();
    for g in &data {
        for n in g {
            *seen.entry(*n).or_insert(0) += 1;
        }
    }
    assert_eq!(seen.len(), 100, "each sample on UDP");
    assert!(seen.values().all(|&c| c <= 3), "at most two copies");
    assert!(
        seen.values().filter(|&&c| c >= 2).count() >= 90,
        "the copies go out"
    );
    assert_eq!(
        idle.first(),
        Some(&100),
        "an idle datagram gives the next number"
    );
    assert!(
        c.tcp(Duration::from_millis(50)).is_empty(),
        "no sample on TCP"
    );
}

#[test]
fn a_hello_with_a_wrong_cookie_keeps_the_samples_on_tcp() {
    let Some(out) = outlet("DgramOut2", Format::Float32) else {
        eprintln!("no local network; skipping");
        return;
    };
    let mut c = Conn::open(&out);
    c.hello(c.param.cookie ^ 1);
    std::thread::sleep(Duration::from_millis(1200));
    push(&out, 0, 10, 5);
    let got = c.tcp(Duration::from_millis(300));
    let expect: Vec<Tcp> = std::iter::once(Tcp::Seq(0))
        .chain((0..10).map(Tcp::Sample))
        .collect();
    assert_eq!(got, expect);
    let (data, idle) = c.udp(Duration::from_millis(100));
    assert!(data.is_empty() && idle.is_empty());
}

#[test]
fn a_resend_request_gets_the_samples_on_tcp() {
    let Some(out) = outlet("DgramOut3", Format::Float32) else {
        eprintln!("no local network; skipping");
        return;
    };
    let (mut c, _) = switched(&out);
    push(&out, 0, 50, 10);
    let _ = c.udp(Duration::from_millis(100));
    let mut r = Vec::new();
    dgram::write_resend(&mut r, 20, 5);
    c.request(&r);
    let got = c.tcp(Duration::from_millis(300));
    let expect: Vec<Tcp> = std::iter::once(Tcp::Seq(20))
        .chain((20..25).map(Tcp::Sample))
        .collect();
    assert_eq!(got, expect);
}

#[test]
fn a_path_request_moves_the_samples_back_to_tcp() {
    let Some(out) = outlet("DgramOut4", Format::Float32) else {
        eprintln!("no local network; skipping");
        return;
    };
    let (mut c, _) = switched(&out);
    push(&out, 0, 30, 10);
    let _ = c.udp(Duration::from_millis(100));
    let mut r = Vec::new();
    dgram::write_path_request(&mut r, Path::Tcp, 25);
    c.request(&r);
    std::thread::sleep(Duration::from_millis(100));
    push(&out, 30, 40, 10);
    let got = c.tcp(Duration::from_millis(300));
    let mut expect = vec![Tcp::Path(Path::Tcp, 25), Tcp::Seq(25)];
    expect.extend((25..40).map(Tcp::Sample));
    assert_eq!(got, expect);
    let (data, _) = c.udp(Duration::from_millis(100));
    assert!(data.is_empty(), "no data on UDP after the fallback");
}

#[test]
fn a_sample_too_large_for_a_datagram_goes_on_tcp() {
    let Some(out) = outlet("DgramOut5", Format::String) else {
        eprintln!("no local network; skipping");
        return;
    };
    let (mut c, _) = switched(&out);
    // The value of a string sample, in these tests, is its length.
    for len in [10usize, 2000, 20] {
        out.push(&Sample {
            timestamp: 0.0,
            values: vec![Value::Str(vec![b'x'; len])],
        });
        std::thread::sleep(Duration::from_millis(20));
    }
    let (data, _) = c.udp(Duration::from_millis(200));
    let on_udp: BTreeSet<u64> = data.into_iter().flatten().collect();
    assert_eq!(on_udp, BTreeSet::from([0, 2]));
    assert_eq!(
        c.tcp(Duration::from_millis(100)),
        vec![Tcp::Seq(1), Tcp::Sample(2000)]
    );
}

#[test]
fn the_udp_port_is_in_the_lsl_port_range() {
    let Some(out) = outlet("DgramOut6", Format::Float32) else {
        eprintln!("no local network; skipping");
        return;
    };
    let c = Conn::open(&out);
    let cfg = labstream_net::config::get();
    assert!((cfg.base_port..cfg.base_port + cfg.port_range).contains(&c.param.port));
}
