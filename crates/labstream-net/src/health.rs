//! Stream health snapshots and gap tracking for inlets and outlets.
//!
//! `docs/health.md` and `docs/roadmap.md` (step 11) specify each field.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free tracker of the most recent sample timestamp and local arrival time.
///
/// Updated once per read batch with two relaxed atomic stores; latency is
/// computed only when [`crate::Inlet::health`] is called.
#[derive(Debug)]
pub(crate) struct LatencyProbe {
    sample_ts_bits: AtomicU64,
    arrival_local_bits: AtomicU64,
}

impl Default for LatencyProbe {
    fn default() -> Self {
        Self {
            sample_ts_bits: AtomicU64::new(f64::NAN.to_bits()),
            arrival_local_bits: AtomicU64::new(f64::NAN.to_bits()),
        }
    }
}

impl LatencyProbe {
    pub(crate) fn record(&self, sample_ts: f64) {
        if !sample_ts.is_finite() {
            return;
        }
        let arrival = crate::clock();
        self.sample_ts_bits
            .store(sample_ts.to_bits(), Ordering::Relaxed);
        self.arrival_local_bits
            .store(arrival.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn measure(&self, clock_offset: Option<f64>) -> Option<f64> {
        let offset = clock_offset?;
        let sample_ts = f64::from_bits(self.sample_ts_bits.load(Ordering::Relaxed));
        let arrival = f64::from_bits(self.arrival_local_bits.load(Ordering::Relaxed));
        if sample_ts.is_finite() && arrival.is_finite() {
            Some((arrival - (sample_ts + offset)).max(0.0))
        } else {
            None
        }
    }
}

/// Maximum number of gaps retained per ring so long streams use bounded memory.
pub(crate) const MAX_GAPS: usize = 1024;

/// Why a gap occurred in the sample stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    /// Samples never reached this inlet (outlet queue drop, network loss, or
    /// dropout not covered by the resume history). Reported through `seq/1` or
    /// `dgram/1`.
    Lost,
    /// Samples arrived at this inlet and were dropped because the inlet buffer
    /// filled before the application pulled them.
    Dropped,
}

/// One missing interval in a stream, given as a pair of timestamps.
///
/// `start` is the timestamp of the last sample before the gap (or the first
/// dropped sample when nothing was read before it). `end` is the timestamp of
/// the first sample after the gap. When `Post::CLOCK_SYNC` is enabled on the
/// inlet and a clock offset exists, both timestamps are shifted to the local
/// clock to match pulled samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gap {
    /// Timestamp at the start of the gap.
    pub start: f64,
    /// Timestamp at the end of the gap (first sample after the missing block).
    pub end: f64,
    /// Number of missing samples in this gap.
    pub count: u64,
    /// Whether the gap came from network/outlet loss or inlet buffer overflow.
    pub kind: GapKind,
}

/// Snapshot of the health of one [`crate::Inlet`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InletHealth {
    /// Measured arrival latency of the most recent sample, in seconds
    /// (`arrival_local - (sample_timestamp + clock_offset)`), or `None` before
    /// the first sample arrives or before clock sync completes.
    pub latency: Option<f64>,
    /// Samples that never reached this inlet (`seq/1`), or `None` when the
    /// source does not use `seq/1`.
    pub lost: Option<u64>,
    /// Samples dropped by this inlet's local buffer because the application
    /// pulled too slowly.
    pub dropped: u64,
    /// Samples delivered from the outlet's `resume/1` history after a dropout,
    /// or `None` when `resume/1` is not active.
    pub resent: Option<u64>,
    /// Round-trip time of the latest clock offset estimate, in seconds, or
    /// `None` before the first burst completes.
    pub clock_uncertainty: Option<f64>,
    /// Number of times the inlet rebuilt its connection.
    pub recoveries: u64,
    /// True while the inlet reader is attached to a source.
    pub connected: bool,
    /// True while samples arrive over UDP (`dgram/1`).
    pub on_udp: bool,
    /// Number of samples currently waiting in the inlet buffer.
    pub buffered: usize,
    /// Capacity of the inlet buffer, in samples.
    pub buffer_capacity: usize,
    /// Number of gaps currently recorded on this inlet.
    pub gap_count: usize,
}

/// Snapshot of one consumer queue on an [`crate::Outlet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerHealth {
    /// Monotonic identifier assigned when the consumer connected.
    pub id: u64,
    /// Remote address of the consumer, when known.
    pub peer: String,
    /// Number of samples currently waiting in this consumer's queue.
    pub fill: usize,
    /// Maximum capacity of this consumer's queue, in samples. Zero for a
    /// blocking outlet, which holds no queue.
    pub capacity: usize,
    /// Number of samples dropped from this consumer's queue because it filled.
    pub dropped: u64,
}

/// Snapshot of the health of one [`crate::Outlet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutletHealth {
    /// Fill and drop state of each connected consumer queue.
    pub consumers: Vec<ConsumerHealth>,
    /// Number of samples currently held in the `resume/1` history.
    pub history_len: usize,
    /// Total number of samples pushed into this outlet since it opened.
    pub pushed: u64,
}

/// Internal ring buffer that tracks [`GapKind::Lost`] intervals from `seq/1`
/// and `dgram/1` delivery order.
#[derive(Debug, Default)]
pub(crate) struct GapTracker {
    /// Timestamp of the last sample delivered on the current outlet run.
    last_ts: Option<f64>,
    /// Pending lost samples waiting for the next sample to close the interval.
    pending_count: u64,
    /// Start timestamp latched when `pending_count` first became non-zero.
    pending_start: Option<f64>,
    /// Completed gaps, bounded to [`MAX_GAPS`].
    gaps: VecDeque<Gap>,
}

impl GapTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record that `delta` samples were lost before the next sample arrives.
    pub(crate) fn record_lost(&mut self, delta: u64) {
        if delta == 0 {
            return;
        }
        if self.pending_count == 0 {
            self.pending_start = self.last_ts;
        }
        self.pending_count = self.pending_count.saturating_add(delta);
    }

    /// Record one delivered sample at raw timestamp `ts`.
    pub(crate) fn on_sample(&mut self, ts: f64) {
        if self.pending_count > 0 {
            let count = self.pending_count;
            let start = self.pending_start.unwrap_or(ts);
            self.pending_count = 0;
            self.pending_start = None;
            // Only record a real gap when a previous sample existed or start is finite.
            if start.is_finite() && ts.is_finite() {
                if self.gaps.len() >= MAX_GAPS {
                    self.gaps.pop_front();
                }
                self.gaps.push_back(Gap {
                    start,
                    end: ts,
                    count,
                    kind: GapKind::Lost,
                });
            }
        }
        self.last_ts = Some(ts);
    }

    /// Record a contiguous batch of delivered samples.
    pub(crate) fn on_batch(&mut self, first_ts: f64, last_ts: f64) {
        self.on_sample(first_ts);
        self.last_ts = Some(last_ts);
    }

    /// Reset the timestamp cursor when attaching to a new outlet run so
    /// timestamps from two different clocks do not pair into a false gap.
    pub(crate) fn reset_cursor(&mut self) {
        self.last_ts = None;
        self.pending_count = 0;
        self.pending_start = None;
    }

    /// Number of recorded gaps.
    pub(crate) fn len(&self) -> usize {
        self.gaps.len()
    }

    /// Snapshot all recorded gaps, adding `shift` to each timestamp.
    pub(crate) fn snapshot(&self, shift: f64) -> Vec<Gap> {
        self.gaps
            .iter()
            .map(|g| Gap {
                start: g.start + shift,
                end: g.end + shift,
                count: g.count,
                kind: g.kind,
            })
            .collect()
    }

    /// Drain all recorded gaps, adding `shift` to each timestamp.
    pub(crate) fn take(&mut self, shift: f64) -> Vec<Gap> {
        self.gaps
            .drain(..)
            .map(|g| Gap {
                start: g.start + shift,
                end: g.end + shift,
                count: g.count,
                kind: g.kind,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_records_exact_timestamp_pair_and_coalesces_consecutive_losses() {
        let mut t = GapTracker::new();
        t.on_sample(10.0);
        t.on_sample(10.5);
        t.record_lost(3);
        t.record_lost(2);
        t.on_sample(13.0);

        let gaps = t.snapshot(0.0);
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            gaps[0],
            Gap {
                start: 10.5,
                end: 13.0,
                count: 5,
                kind: GapKind::Lost,
            }
        );

        // Clock shift moves both endpoints by the same offset.
        let shifted = t.take(100.0);
        assert_eq!(shifted[0].start, 110.5);
        assert_eq!(shifted[0].end, 113.0);
        assert!(t.snapshot(0.0).is_empty());
    }

    #[test]
    fn tracker_bounds_ring_size_and_resets_cursor_on_new_run() {
        let mut t = GapTracker::new();
        t.on_sample(0.0);
        for i in 1..=(MAX_GAPS + 20) {
            t.record_lost(1);
            t.on_sample(i as f64);
        }
        assert_eq!(t.len(), MAX_GAPS);
        let first = t.snapshot(0.0)[0];
        assert_eq!(first.end, 21.0);

        // Pending loss across a new outlet run does not pair old and new clocks.
        t.record_lost(10);
        t.reset_cursor();
        t.on_sample(5000.0);
        assert_eq!(t.len(), MAX_GAPS);
    }
}
