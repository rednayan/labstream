//! Helpers for the `resume/1` tests. `docs/extensions.md`, section `resume`.
//!
//! Each test file that needs its own configuration is its own process, and
//! each one uses some of these helpers.

#![allow(dead_code)]

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_proto::features::{self, ResumeParam, Token};
use labstream_proto::feed::{FeedClient, FeedParams, TestPatternError};
use labstream_proto::seq::{self, Next};
use labstream_wire::{Codec, Format, Sample, TimestampDeducer, Value};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Use the configuration file with this text for the whole test process.
/// Call it before any other call of the library, because the library reads
/// its configuration one time.
pub fn use_configuration(name: &str, text: &str) {
    let path = std::env::temp_dir().join(format!("labstream-{name}-{}.cfg", std::process::id()));
    std::fs::write(&path, text).expect("a configuration file");
    std::env::set_var("LSLAPICFG", &path);
}

pub const RATE: f64 = 1000.0;

pub fn outlet(name: &str) -> Option<Outlet> {
    let mut info = StreamInfo::new(name, "ResumeTest", 1, Format::Float32, RATE);
    info.source_id = format!("{name}_src");
    Outlet::new(info).ok()
}

/// Push the samples `from..to` in chunks of `len`, each chunk with one
/// timestamp. SPEC.md 9.2.
pub fn push(out: &Outlet, from: usize, to: usize, len: usize) {
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
pub struct Feed {
    stream: TcpStream,
    codec: Codec,
    buf: Vec<u8>,
    deducer: TimestampDeducer,
    pub answer: Vec<Token>,
}

/// A received sample: its value, its wire timestamp, and its full timestamp.
pub type Got = (usize, f64, u64);

impl Feed {
    pub fn open(out: &Outlet, offer: Vec<Token>) -> Feed {
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
    pub fn read(&mut self, count: usize) -> (Vec<u64>, Vec<Got>) {
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

pub fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>) {
    let mut tmp = [0u8; 65536];
    match stream.read(&mut tmp) {
        Ok(0) => panic!("the outlet closed the connection"),
        Ok(n) => buf.extend_from_slice(&tmp[..n]),
        Err(_) => {}
    }
}

pub fn offer(param: Option<ResumeParam>) -> Vec<Token> {
    let allowed = vec![features::SEQ.to_string(), features::RESUME.to_string()];
    let mut tokens = features::offer(&allowed);
    tokens.extend(features::offer_resume(&allowed, param));
    tokens
}

pub fn values(samples: &[Got]) -> Vec<usize> {
    samples.iter().map(|s| s.0).collect()
}

/// A TCP proxy that forwards each connection to the target port, and can
/// cut them.
pub struct Proxy {
    pub port: u16,
    target: Arc<AtomicU16>,
    accepted: Arc<Mutex<Vec<TcpStream>>>,
    /// While true, the proxy closes each new connection at once.
    pub paused: Arc<AtomicBool>,
}

impl Proxy {
    pub fn start(target: u16) -> Option<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let list = Arc::clone(&accepted);
        let paused = Arc::new(AtomicBool::new(false));
        let hold = Arc::clone(&paused);
        let target = Arc::new(AtomicU16::new(target));
        let to = Arc::clone(&target);
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                if hold.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let server = match TcpStream::connect(("127.0.0.1", to.load(Ordering::SeqCst))) {
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
            target,
            accepted,
            paused,
        })
    }

    /// Send the next connections to a different port.
    pub fn retarget(&self, port: u16) {
        self.target.store(port, Ordering::SeqCst);
    }

    /// Cut each connection that is open now.
    pub fn cut(&self) {
        for s in self.accepted.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// Cut each connection, and refuse new ones for `time`.
    pub fn cut_for(&self, time: Duration) {
        self.paused.store(true, Ordering::SeqCst);
        self.cut();
        std::thread::sleep(time);
        self.paused.store(false, Ordering::SeqCst);
    }

    /// The description of `real`, with the address of this proxy.
    pub fn info(&self, real: &StreamInfo) -> StreamInfo {
        let mut via = real.clone();
        via.v4address = "127.0.0.1".to_string();
        via.v4data_port = self.port;
        via
    }
}

pub fn pipe(mut from: TcpStream, mut to: TcpStream) {
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
pub fn index(s: &Sample) -> usize {
    match &s.values[0] {
        Value::F32(v) => *v as usize,
        Value::Str(b) => String::from_utf8_lossy(b).parse().unwrap(),
        _ => panic!("wrong format"),
    }
}

/// Pull until a sample with the index `last` arrives, or the time runs out.
pub fn pull_until(inlet: &mut Inlet, got: &mut Vec<Sample>, last: usize, within: Duration) {
    let end = Instant::now() + within;
    while Instant::now() < end && got.last().map(index) != Some(last) {
        let _ = inlet.pull_chunk(got, 1_000_000, Duration::from_millis(20));
    }
}

/// One dropout run. The outlet pushes chunks of `chunk` samples at `rate`,
/// and the proxy cuts the connection of one inlet for `cut`. A second inlet
/// connects directly.
pub struct DropSpec {
    pub name: &'static str,
    pub format: Format,
    pub channels: usize,
    /// The nominal rate. A rate of 0 is a marker stream, with one marker
    /// each 5 ms.
    pub rate: f64,
    pub chunk: usize,
    pub cut: Duration,
    /// Turn on each post-processing stage on both inlets.
    pub post: bool,
}

/// What a dropout run gave.
pub struct DropRun {
    /// The samples of the inlet behind the proxy.
    pub got: Vec<Sample>,
    /// The samples of the inlet that connects directly.
    pub reference: Vec<Sample>,
    /// The index of the last pushed sample.
    pub last: usize,
    pub lost: Option<u64>,
    pub resent: Option<u64>,
    pub dropped: u64,
    pub recoveries: u64,
    /// The longest push call after the end of the cut.
    pub longest_push_after_cut: Duration,
}

/// Run one dropout. The result is `None` with no local network.
pub fn run_dropout(spec: &DropSpec) -> Option<DropRun> {
    let mut info = StreamInfo::new(
        spec.name,
        "ResumeTest",
        spec.channels as u32,
        spec.format,
        spec.rate,
    );
    info.source_id = format!("{}_src", spec.name);
    let out = Arc::new(Outlet::new(info).ok()?);
    let real = out.info().clone();
    let proxy = Proxy::start(real.v4data_port)?;
    let mut cut = Inlet::open_recovering(&proxy.info(&real), Duration::from_secs(5), 1_000_000)
        .expect("the inlet connects through the proxy");
    let mut direct = Inlet::open_recovering(&real, Duration::from_secs(5), 1_000_000)
        .expect("the inlet connects directly");
    if spec.post {
        cut.set_postprocessing_all();
        direct.set_postprocessing_all();
    }
    let start = Instant::now();
    while out.consumer_count() < 2 && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        cut.samples_resent(),
        Some(0),
        "the connection uses resume/1"
    );

    let pushed = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let after_cut = Arc::new(AtomicBool::new(false));
    let longest = Arc::new(Mutex::new(Duration::ZERO));
    let (format, channels, rate, chunk) = (spec.format, spec.channels, spec.rate, spec.chunk);
    let source = {
        let (out, pushed, stop) = (Arc::clone(&out), Arc::clone(&pushed), Arc::clone(&stop));
        let (after_cut, longest) = (Arc::clone(&after_cut), Arc::clone(&longest));
        std::thread::spawn(move || {
            let period = if rate > 0.0 {
                Duration::from_secs_f64(chunk as f64 / rate)
            } else {
                Duration::from_millis(5)
            };
            let mut k = 0;
            let mut next = Instant::now();
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
                let t = Instant::now();
                out.push_chunk(&block, 0.0);
                let took = t.elapsed();
                if after_cut.load(Ordering::SeqCst) {
                    let mut l = longest.lock().unwrap();
                    *l = (*l).max(took);
                }
                k += chunk;
                pushed.store(k, Ordering::SeqCst);
                // An absolute schedule, so a slow push does not slow the rate.
                next += period;
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
        })
    };

    let mut got = Vec::new();
    let mut reference = Vec::new();
    std::thread::sleep(Duration::from_millis(500));
    let _ = cut.pull_chunk(&mut got, 1_000_000, Duration::from_millis(20));
    let _ = direct.pull_chunk(&mut reference, 1_000_000, Duration::from_millis(20));
    assert!(!got.is_empty(), "samples arrive before the cut");
    // The direct inlet keeps reading during the cut, so that its own queue
    // does not fill.
    proxy.paused.store(true, Ordering::SeqCst);
    proxy.cut();
    let end = Instant::now() + spec.cut;
    while Instant::now() < end {
        let _ = direct.pull_chunk(&mut reference, 1_000_000, Duration::from_millis(20));
    }
    proxy.paused.store(false, Ordering::SeqCst);
    after_cut.store(true, Ordering::SeqCst);
    let settle = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < settle {
        let _ = direct.pull_chunk(&mut reference, 1_000_000, Duration::from_millis(20));
        let _ = cut.pull_chunk(&mut got, 1_000_000, Duration::from_millis(1));
    }
    stop.store(true, Ordering::SeqCst);
    source.join().unwrap();
    let last = pushed.load(Ordering::SeqCst) - 1;
    pull_until(&mut cut, &mut got, last, Duration::from_secs(15));
    pull_until(&mut direct, &mut reference, last, Duration::from_secs(5));
    let longest_push_after_cut = *longest.lock().unwrap();
    Some(DropRun {
        got,
        reference,
        last,
        lost: cut.samples_lost(),
        resent: cut.samples_resent(),
        dropped: cut.samples_dropped(),
        recoveries: cut.recoveries(),
        longest_push_after_cut,
    })
}
