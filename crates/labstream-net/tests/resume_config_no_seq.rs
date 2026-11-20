//! `labstream.Features = {resume}` without `seq`. `docs/extensions.md`,
//! section `resume`, conformance item 5.
//!
//! The library reads its configuration one time, so this file is its own
//! test process.

mod resume_common;

use labstream_net::Inlet;
use labstream_proto::features::{self, Token};
use resume_common::*;
use std::time::Duration;

#[test]
fn resume_without_seq_is_off_on_both_sides() {
    use_configuration("no-seq", "[labstream]\nFeatures = {resume}\n");
    let Some(out) = outlet("ResumeNoSeq1") else {
        eprintln!("no local network; skipping");
        return;
    };
    // The outlet side: an offer of both gets no answer, because this
    // process does not allow `seq`.
    let feed = Feed::open(
        &out,
        vec![
            Token::new(features::SEQ, 1),
            Token::new(features::RESUME, 1),
        ],
    );
    assert!(feed.answer.is_empty(), "the answer: {:?}", feed.answer);
    drop(feed);

    // The inlet side: the inlet offers nothing, so both counts are unknown.
    let inlet = Inlet::open(out.info(), Duration::from_secs(5)).expect("an inlet");
    assert_eq!(inlet.samples_lost(), None);
    assert_eq!(inlet.samples_resent(), None);
    assert_eq!(out.history_len(), 0);
}
