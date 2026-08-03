//! The whole API, over a real socket, with nothing but the `labstream` crate.
//!
//! If a program can do this, the API is complete: publish, discover, read the
//! channels, and read a block of samples.
use labstream::{Buffer, Channel, Chunk, Format, Inlet, Outlet, Post, Query, StreamInfo};
use std::time::Duration;

const RATE: f64 = 250.0;
const CH: usize = 4;

#[test]
fn a_program_uses_only_this_crate() {
    // ── publish ─────────────────────────────────────────────────────────────
    let info = StreamInfo::builder("FacadeTest", "EEG", Format::Float32)
        .rate(RATE)
        .source_id("facade-test-1")
        .channels([
            Channel::new("Fp1").unit("microvolts").kind("EEG"),
            Channel::new("Fp2").unit("microvolts").kind("EEG"),
            Channel::new("C3").unit("microvolts").kind("EEG"),
            Channel::new("C4").unit("microvolts").kind("EEG"),
        ])
        .build()
        .expect("a description");
    assert_eq!(info.channel_count(), CH, "the channels set the count");
    let outlet = Outlet::new(info).expect("an outlet");

    // ── discover ────────────────────────────────────────────────────────────
    let found = labstream::resolve_all(&Query::stream_type("EEG"), Duration::from_secs(2))
        .expect("a resolve");
    let mine = found
        .iter()
        .find(|s| s.source_id() == "facade-test-1")
        .expect("the stream is on the network");
    assert_eq!(mine.rate(), RATE);
    assert!(mine.is_regular());

    // ── read the channels back off the wire ─────────────────────────────────
    let full = mine
        .fetch(Duration::from_secs(2))
        .expect("the full description");
    let channels = full.channels();
    assert_eq!(
        channels
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        ["Fp1", "Fp2", "C3", "C4"]
    );
    assert_eq!(channels[0].unit, "microvolts");
    assert_eq!(channels[0].kind, "EEG");

    // ── read samples ────────────────────────────────────────────────────────
    let mut inlet = Inlet::builder(mine)
        .buffer(Buffer::Seconds(8.0))
        .postprocess(Post::ALL)
        .open(Duration::from_secs(5))
        .expect("an inlet");
    assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

    // 200 samples, written as one block, per channel a distinct ramp.
    let n = 200usize;
    let data: Vec<f32> = (0..n)
        .flat_map(|k| (0..CH).map(move |c| (k * 10 + c) as f32))
        .collect();
    outlet
        .push_chunk(&data, labstream::clock())
        .expect("a block write");

    let mut chunk = Chunk::<f32>::new(CH, 4096);
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while chunk.len() < n && std::time::Instant::now() < end {
        inlet
            .pull_chunk(&mut chunk, Duration::from_millis(100))
            .expect("a block read");
    }
    assert_eq!(chunk.len(), n, "every sample arrived");

    // Values are in place, sample by sample.
    for (k, (_t, sample)) in chunk.iter().enumerate() {
        for (c, v) in sample.iter().enumerate() {
            assert_eq!(*v, (k * 10 + c) as f32, "sample {k}, channel {c}");
        }
    }

    // And the strided read gives one channel with no transpose loop.
    let ch2: Vec<f32> = chunk.channel(2).collect();
    assert_eq!(ch2.len(), n);
    assert_eq!(ch2[0], 2.0);
    assert_eq!(ch2[n - 1], ((n - 1) * 10 + 2) as f32);

    // Timestamps stay ordered under the post-processing stages.
    let stamps = chunk.timestamps();
    assert!(stamps.windows(2).all(|w| w[1] > w[0]), "monotonic");
}

#[test]
fn a_wrong_sample_length_is_an_error_not_a_bad_read() {
    let info = StreamInfo::builder("FacadeLen", "EEG", Format::Float32)
        .rate(RATE)
        .source_id("facade-len-1")
        .channel_count(4)
        .build()
        .expect("a description");
    let outlet = Outlet::new(info).expect("an outlet");
    assert!(outlet.push(&[1.0f32, 2.0]).is_err(), "a short sample fails");
    assert!(outlet.push(&[1.0f32, 2.0, 3.0, 4.0]).is_ok());
}

#[test]
fn a_description_that_disagrees_with_itself_does_not_build() {
    let bad = StreamInfo::builder("Bad", "EEG", Format::Float32)
        .channel_count(8)
        .channel_labels(["a", "b"])
        .build();
    assert!(bad.is_err(), "8 channels with 2 labels must not build");
}

#[test]
fn a_watcher_finds_a_stream_without_a_wait() {
    let info = StreamInfo::builder("FacadeWatch", "Watched", Format::Float32)
        .rate(RATE)
        .source_id("facade-watch-1")
        .channel_count(1)
        .build()
        .expect("a description");
    let _outlet = Outlet::new(info).expect("an outlet");

    let watcher = labstream::Watcher::new(&Query::stream_type("Watched"), Duration::from_secs(5));
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while watcher.is_empty() && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
    let seen = watcher.streams();
    assert!(
        seen.iter().any(|s| s.source_id() == "facade-watch-1"),
        "the watcher holds the stream"
    );

    // The read itself must not wait, because a browser calls it every frame.
    let t = std::time::Instant::now();
    let _ = watcher.streams();
    assert!(
        t.elapsed() < Duration::from_millis(50),
        "the read is immediate"
    );
}

#[test]
fn a_marker_stream_goes_out_and_comes_back_as_text() {
    // A marker stream is irregular and carries one string for each event. It is
    // the one stream shape that `pull` cannot read, because `Scalar` is `Copy`.
    let info = StreamInfo::builder("FacadeMarkers", "Markers", Format::String)
        .rate(0.0)
        .source_id("facade-markers-1")
        .channel_count(1)
        .build()
        .expect("a description");
    let outlet = Outlet::new(info).expect("an outlet");

    let found = lsl_resolve("source_id='facade-markers-1'");
    let mut inlet = Inlet::builder(&found)
        .buffer(Buffer::Samples(100))
        .open(Duration::from_secs(5))
        .expect("an inlet");

    let sent = ["trial start", "cue", "response"];
    for m in sent {
        outlet.push_text(m).expect("a marker");
    }

    let mut got = Vec::new();
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while got.len() < sent.len() && std::time::Instant::now() < end {
        if let Some((at, values)) = inlet.pull_text(Duration::from_millis(100)).expect("a pull") {
            assert_eq!(values.len(), 1, "a marker stream has one channel");
            assert!(at > 0.0, "the marker carries a timestamp");
            got.push(values.into_iter().next().unwrap());
        }
    }
    assert_eq!(got, sent, "every marker arrives, in order");
}

/// Resolve one stream by predicate, or fail the test.
fn lsl_resolve(pred: &str) -> StreamInfo {
    labstream::resolve_first(&Query::predicate(pred), Duration::from_secs(5))
        .expect("a resolve")
        .expect("the stream")
}
