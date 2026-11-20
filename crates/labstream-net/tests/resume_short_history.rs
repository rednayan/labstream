//! A dropout that is longer than the `resume` history. `docs/extensions.md`,
//! section `resume`, conformance item 11.
//!
//! The test sets `labstream.ResumeSeconds = 1`. The library reads its
//! configuration one time, so this file is its own test process.

mod resume_common;

use labstream_wire::Format;
use resume_common::*;
use std::time::Duration;

#[test]
fn the_loss_count_is_exact_when_the_history_is_too_short() {
    use_configuration("short-history", "[labstream]\nResumeSeconds = 1\n");
    let spec = DropSpec {
        name: "ResumeShort1",
        format: Format::Float32,
        channels: 8,
        rate: 512.0,
        chunk: 8,
        cut: Duration::from_secs(3),
        post: false,
    };
    let Some(run) = run_dropout(&spec) else {
        eprintln!("no local network; skipping");
        return;
    };
    assert!(run.recoveries >= 1, "the inlet connected again");
    let indices: Vec<usize> = run.got.iter().map(index).collect();
    assert!(
        indices.windows(2).all(|w| w[1] > w[0]),
        "in order, and no sample twice"
    );
    let first = indices[0];
    let missing = (run.last - first + 1 - indices.len()) as u64;
    let resent = run.resent.expect("the connection uses resume/1");
    println!(
        "{} samples, {missing} missing, lost {:?}, resent {resent}",
        indices.len(),
        run.lost
    );
    // The history holds 1 s, so most of the 3 s cut is lost. The count of
    // lost samples is exact, and the history gives about 1 s.
    assert!(missing > 512, "the dropout is longer than the history");
    assert_eq!(run.lost, Some(missing), "the loss count is exact");
    assert_eq!(run.dropped, 0);
    assert!(
        (400..=600).contains(&resent),
        "about 1 s came from the history: {resent}"
    );
}
