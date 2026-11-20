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

mod resume_common;

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_proto::features::{self, ResumeParam, Token};
use labstream_wire::{Format, DEDUCED_TIMESTAMP};
use resume_common::*;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

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

/// The checks of conformance item 9: each sample once, in order, nothing
/// lost, and the samples of the dropout from the history.
fn check_lossless(spec: &DropSpec, run: &DropRun) {
    assert!(run.recoveries >= 1, "the inlet connected again");
    let first = index(&run.got[0]);
    let indices: Vec<usize> = run.got.iter().map(index).collect();
    assert_eq!(
        indices,
        (first..=run.last).collect::<Vec<_>>(),
        "each sample once, in order"
    );
    assert_eq!(run.lost, Some(0));
    assert_eq!(run.dropped, 0);
    let resent = run.resent.expect("the connection uses resume/1");
    // At least two thirds of the cut must come from the history. A marker
    // stream pushes 200 markers each second.
    let rate = if spec.rate > 0.0 { spec.rate } else { 200.0 };
    let at_least = rate * spec.cut.as_secs_f64() * 2.0 / 3.0;
    assert!(
        resent as f64 >= at_least,
        "the history gave the samples of the dropout: {resent}"
    );
    println!(
        "{}: {} samples, {resent} resent, {} recoveries, longest push after the cut {:?}",
        spec.name,
        run.got.len(),
        run.recoveries,
        run.longest_push_after_cut
    );
}

/// The same values and timestamps as the direct inlet, bit for bit.
fn check_same_as_direct(run: &DropRun) {
    let first = index(&run.got[0]);
    let offset = first - index(&run.reference[0]);
    for (a, b) in run.got.iter().zip(&run.reference[offset..]) {
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

fn spec(name: &'static str, format: Format, channels: usize, rate: f64, chunk: usize) -> DropSpec {
    DropSpec {
        name,
        format,
        channels,
        rate,
        chunk,
        cut: Duration::from_secs(3),
        post: false,
    }
}

#[test]
fn a_dropout_of_3_s_at_512_hz_loses_nothing() {
    let s = spec("ResumeDrop1", Format::Float32, 8, 512.0, 8);
    let Some(run) = run_dropout(&s) else {
        eprintln!("no local network; skipping");
        return;
    };
    check_lossless(&s, &run);
    check_same_as_direct(&run);
}

#[test]
fn a_dropout_of_3_s_at_4000_hz_and_64_channels_loses_nothing() {
    let s = spec("ResumeDrop2", Format::Float32, 64, 4000.0, 40);
    let Some(run) = run_dropout(&s) else {
        eprintln!("no local network; skipping");
        return;
    };
    check_lossless(&s, &run);
    check_same_as_direct(&run);
}

#[test]
fn a_dropout_of_3_s_on_a_marker_stream_loses_nothing() {
    let s = spec("ResumeDrop3", Format::String, 2, 0.0, 1);
    let Some(run) = run_dropout(&s) else {
        eprintln!("no local network; skipping");
        return;
    };
    check_lossless(&s, &run);
    check_same_as_direct(&run);
}

#[test]
fn a_dropout_with_post_processing_keeps_a_smooth_time_axis() {
    // Each inlet measures its own clock offset, so two inlets with clock
    // sync do not agree bit for bit. The check is thus on the time axis of
    // the cut inlet: it never goes back, and no step across the dropout is
    // more than two sample periods. `docs/extensions.md`, conformance
    // item 9.
    let mut s = spec("ResumeDrop4", Format::Float32, 8, 512.0, 8);
    s.post = true;
    let Some(run) = run_dropout(&s) else {
        eprintln!("no local network; skipping");
        return;
    };
    check_lossless(&s, &run);
    let period = 1.0 / s.rate;
    let mut widest: f64 = 0.0;
    for pair in run.got.windows(2) {
        let step = pair[1].timestamp - pair[0].timestamp;
        assert!(
            step >= 0.0,
            "the time goes back at sample {}",
            index(&pair[1])
        );
        widest = widest.max(step);
    }
    assert!(
        widest <= 2.0 * period,
        "the widest step is {widest} s, the period is {period} s"
    );
}

#[test]
fn a_plain_request_starts_no_history() {
    // A liblsl inlet sends no `Labstream-Features` line, and an older
    // labstream inlet offers `seq/1` only. Neither starts the history.
    // `docs/extensions.md`, conformance item 2.
    let Some(out) = outlet("ResumeLive5") else {
        eprintln!("no local network; skipping");
        return;
    };
    let plain = Feed::open(&out, Vec::new());
    let seq_only = Feed::open(&out, vec![Token::new(features::SEQ, 1)]);
    push(&out, 0, 100, 10);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(out.history_len(), 0);
    drop((plain, seq_only));

    let _resume = Feed::open(&out, offer(None));
    push(&out, 100, 150, 10);
    assert_eq!(out.history_len(), 50, "the history starts with resume/1");
}

#[test]
fn a_new_outlet_with_an_old_uid_gives_no_resend_and_no_false_loss() {
    // A relay or a replay can open a new outlet with an old `uid`. The new
    // outlet has a new epoch, so the inlet must reset its count, and must get
    // nothing again. `docs/extensions.md`, conformance item 10.
    let mut info = StreamInfo::new("ResumeLive6", "ResumeTest", 1, Format::Float32, RATE);
    info.source_id = "ResumeLive6_src".to_string();
    info.uid = "resume-live-6-reused-uid".to_string();
    let Ok(first) = Outlet::new(info.clone()) else {
        eprintln!("no local network; skipping");
        return;
    };
    let real = first.info().clone();
    let Some(proxy) = Proxy::start(real.v4data_port) else {
        eprintln!("no local network; skipping");
        return;
    };
    let mut inlet = Inlet::open_recovering(&proxy.info(&real), Duration::from_secs(5), 100_000)
        .expect("the inlet connects through the proxy");
    assert!(first.wait_for_consumers(Duration::from_secs(5)));
    push(&first, 0, 100, 10);
    let mut got = Vec::new();
    pull_until(&mut inlet, &mut got, 99, Duration::from_secs(5));
    assert_eq!(got.len(), 100);

    // The first outlet closes. A new one opens with the same `uid`, and
    // pushes 300 samples before the inlet comes back. Its counter is then
    // at 300, and the inlet expects 100.
    proxy.paused.store(true, Ordering::SeqCst);
    drop(first);
    proxy.cut();
    let second = Outlet::new(info).expect("a second outlet");
    assert_eq!(second.info().uid, real.uid);
    proxy.retarget(second.info().v4data_port);
    push(&second, 1000, 1300, 10);
    proxy.paused.store(false, Ordering::SeqCst);

    let end = Instant::now() + Duration::from_secs(10);
    while inlet.recoveries() == 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(inlet.recoveries() >= 1, "the inlet connected again");
    std::thread::sleep(Duration::from_millis(200));
    push(&second, 1300, 1350, 10);
    let mut after = Vec::new();
    pull_until(&mut inlet, &mut after, 1349, Duration::from_secs(5));
    let values: Vec<usize> = after.iter().map(index).collect();
    assert_eq!(
        values,
        (1300..1350).collect::<Vec<_>>(),
        "only the live samples"
    );
    assert_eq!(inlet.samples_lost(), Some(0), "no false loss");
    assert_eq!(inlet.samples_resent(), Some(0), "nothing again");
}

#[test]
#[ignore = "a measurement: run with cargo test --release -- --ignored"]
fn the_longest_push_during_a_long_resend_stays_short() {
    // At 4000 Hz x 64 channels, a cut of 7 s gives a resend of about 7.5 s,
    // which fits in the default memory limit of 64 MiB. A push waits for at
    // most one block of 256 samples. `docs/extensions.md`, conformance
    // item 12. A debug build copies far slower, so the limit applies to a
    // release build only.
    let mut s = spec("ResumeDrop5", Format::Float32, 64, 4000.0, 40);
    s.cut = Duration::from_secs(7);
    let Some(run) = run_dropout(&s) else {
        eprintln!("no local network; skipping");
        return;
    };
    check_lossless(&s, &run);
    assert!(
        run.longest_push_after_cut < Duration::from_millis(2),
        "the longest push after the cut took {:?}",
        run.longest_push_after_cut
    );
}
