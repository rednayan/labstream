//! `labstream.ResumeSeconds = 0` turns the history off. `docs/extensions.md`,
//! section `resume`, conformance item 5.
//!
//! The library reads its configuration one time, so this file is its own
//! test process.

mod resume_common;

use labstream_net::Inlet;
use labstream_proto::features;
use resume_common::*;
use std::time::Duration;

#[test]
fn a_history_of_zero_seconds_gives_no_resume() {
    use_configuration("no-history", "[labstream]\nResumeSeconds = 0\n");
    let Some(out) = outlet("ResumeNoHistory1") else {
        eprintln!("no local network; skipping");
        return;
    };
    let feed = Feed::open(&out, offer(None));
    assert!(features::has(&feed.answer, features::SEQ, 1));
    assert_eq!(features::resume_answer(&feed.answer), None);
    drop(feed);

    let inlet = Inlet::open(out.info(), Duration::from_secs(5)).expect("an inlet");
    assert_eq!(inlet.samples_lost(), Some(0), "seq/1 stays on");
    assert_eq!(inlet.samples_resent(), None, "resume/1 is off");
    push(&out, 0, 100, 10);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(out.history_len(), 0);
}
