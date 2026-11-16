//! The outlet side of the `resume/1` feature, measured against a live
//! outlet. `docs/extensions.md`, section `resume`.
//!
//! These tests open real sockets on the loopback interface, so they belong to
//! the reporting tier and not to the merge gate. A failure here can come from
//! the machine.
//!
//! The inlet side is not wired yet. Each test therefore opens the feed by
//! hand and reads the bytes as an inlet does.

use labstream_net::{Outlet, StreamInfo};
use labstream_proto::features::{self, ResumeParam, Token};
use labstream_proto::feed::{FeedClient, FeedParams, TestPatternError};
use labstream_proto::seq::{self, Next};
use labstream_wire::{Codec, Format, Sample, TimestampDeducer, Value, DEDUCED_TIMESTAMP};
use std::io::{Read, Write};
use std::net::TcpStream;
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
