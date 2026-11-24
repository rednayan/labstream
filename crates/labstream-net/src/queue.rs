//! A bounded sample queue that drops the oldest sample. SPEC.md 8.7.
//!
//! liblsl holds one of these per consumer on the outlet side
//! (`src/consumer_queue.h`), and one on the inlet side for samples that the
//! application has not pulled yet (`src/data_receiver.h:35`). Both drop the
//! oldest sample when they fill.
//!
//! The recorded behavior that this matches is in
//! `artifacts/behavior-liblsl.json`. A ring of 100 samples, fed 600 samples
//! faster than the reader consumed them, delivered sample 0 and then samples
//! 500 to 599. The writer never waited.

use crate::health::{ConsumerHealth, Gap, GapKind, OutletHealth, MAX_GAPS};
use crate::history::{History, HistoryLimits, HistoryRead};
use labstream_wire::{PackedSample, Sample, TimestampDeducer, IRREGULAR_RATE};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The samples of a queue, and the number after the last one.
///
/// Each push adds one to `back`, also for a sample that the queue drops at
/// once. A removal does not change it. The samples in the queue thus always
/// have the numbers `back - len .. back`, with no gap: the queue drops only
/// from the front, and a push only adds at the back. `docs/extensions.md`.
#[derive(Debug)]
struct Ring {
    q: VecDeque<PackedSample>,
    back: u64,
    /// The full timestamp of each sample that leaves the queue, in order.
    /// Only a queue for a `resume` consumer has one. `docs/extensions.md`,
    /// rule 5 of the resend timestamps.
    deducer: Option<TimestampDeducer>,
    /// Timestamp of the last sample taken by the reader, for drop-gap tracking.
    last_popped_ts: Option<f64>,
    /// Samples dropped since the last sample taken by the reader.
    pending_drop_count: u64,
    /// Start timestamp latched when `pending_drop_count` became non-zero.
    pending_drop_start: Option<f64>,
    /// Completed drop gaps, bounded to `MAX_GAPS`.
    drop_gaps: VecDeque<Gap>,
    /// Sparse `(ring_pos, skipped_count)` markers recording wire-lost or
    /// queue-dropped samples immediately before `ring_pos`, for loss-aware
    /// dejitter (`docs/roadmap.md`, step 2). Always empty on a clean stream.
    skips: VecDeque<(u64, u64)>,
}

impl std::ops::Deref for Ring {
    type Target = VecDeque<PackedSample>;
    fn deref(&self) -> &VecDeque<PackedSample> {
        &self.q
    }
}

impl std::ops::DerefMut for Ring {
    fn deref_mut(&mut self) -> &mut VecDeque<PackedSample> {
        &mut self.q
    }
}

impl Ring {
    /// The number of the sample at the front.
    fn front_number(&self) -> u64 {
        self.back.wrapping_sub(self.q.len() as u64)
    }

    /// Record that `count` samples starting near `dropped_ts` were discarded.
    fn note_drop(&mut self, dropped_ts: f64, count: u64) {
        if count == 0 {
            return;
        }
        if self.pending_drop_count == 0 {
            self.pending_drop_start = self.last_popped_ts.or(Some(dropped_ts));
        }
        self.pending_drop_count = self.pending_drop_count.saturating_add(count);
    }

    /// Finalize any pending drop gap when the reader receives a sample at `end_ts`.
    fn note_pop(&mut self, first_ts: f64, last_ts: f64) {
        if self.pending_drop_count > 0 {
            let count = self.pending_drop_count;
            let start = self.pending_drop_start.unwrap_or(first_ts);
            self.pending_drop_count = 0;
            self.pending_drop_start = None;
            if start.is_finite() && first_ts.is_finite() {
                if self.drop_gaps.len() >= MAX_GAPS {
                    self.drop_gaps.pop_front();
                }
                self.drop_gaps.push_back(Gap {
                    start,
                    end: first_ts,
                    count,
                    kind: GapKind::Dropped,
                });
            }
        }
        self.last_popped_ts = Some(last_ts);
    }

    /// Record that `count` samples were skipped immediately before `pos`,
    /// keeping `self.skips` sorted by ascending position.
    fn push_skip(&mut self, pos: u64, count: u64) {
        if count == 0 {
            return;
        }
        if let Some((last_pos, last_count)) = self.skips.back_mut() {
            if *last_pos == pos {
                *last_count = last_count.saturating_add(count);
                return;
            }
            if *last_pos < pos {
                self.skips.push_back((pos, count));
                return;
            }
        } else {
            self.skips.push_back((pos, count));
            return;
        }
        // Front insertion (e.g. `drop_front` while a later wire skip exists).
        if let Some((first_pos, first_count)) = self.skips.front_mut() {
            if *first_pos == pos {
                *first_count = first_count.saturating_add(count);
                return;
            }
            if pos < *first_pos {
                self.skips.push_front((pos, count));
                return;
            }
        }
        for i in 0..self.skips.len() {
            let (p, c) = &mut self.skips[i];
            if *p == pos {
                *c = c.saturating_add(count);
                return;
            }
            if pos < *p {
                self.skips.insert(i, (pos, count));
                return;
            }
        }
        self.skips.push_back((pos, count));
    }

    /// Take all skipped-sample counts that precede `ring_pos`.
    fn take_skips_at(&mut self, ring_pos: u64) -> u64 {
        if self.skips.is_empty() {
            return 0;
        }
        let mut total = 0u64;
        while let Some(&(pos, count)) = self.skips.front() {
            if pos <= ring_pos {
                total = total.saturating_add(count);
                self.skips.pop_front();
            } else {
                break;
            }
        }
        total
    }

    /// Remove the oldest sample because the queue is full.
    ///
    /// The deducer sees the sample, so the full timestamp of the next sample
    /// stays correct after the drop.
    fn drop_front(&mut self) {
        let front_pos = self.front_number();
        if let Some(old) = self.q.pop_front() {
            self.note_drop(old.timestamp, 1);
            // The next surviving sample is at `front_pos + 1`; attach 1 skipped
            // sample there so loss-aware dejitter accounts for inlet drops too.
            self.push_skip(front_pos.wrapping_add(1), 1);
            if let Some(d) = self.deducer.as_mut() {
                d.apply(old.timestamp);
            }
        }
    }
}

/// A queue that never blocks its writer.
#[derive(Debug)]
pub struct SampleQueue {
    inner: Mutex<Ring>,
    room: Condvar,
    capacity: usize,
    dropped: AtomicU64,
    closed: Mutex<bool>,
    /// Set by [`SampleQueue::wake`]. The next wait returns at once.
    woken: std::sync::atomic::AtomicBool,
}

impl SampleQueue {
    /// Build a queue that holds `capacity` samples.
    ///
    /// A capacity of zero would drop every sample, so the value floors at 1.
    /// liblsl floors the same way (`src/stream_info_impl.cpp:268`).
    pub fn new(capacity: usize) -> Self {
        Self::with_first_number(capacity, 0)
    }

    /// Build a queue whose first pushed sample gets the number `first`.
    ///
    /// The `seq` feature numbers the samples of an outlet with one counter.
    /// A consumer that connects later starts at the current count.
    pub fn with_first_number(capacity: usize, first: u64) -> Self {
        Self::build(capacity, first, None)
    }

    /// Build a queue that also gives the full timestamp of each sample.
    ///
    /// `deducer` must hold the state after the sample with the number
    /// `first - 1`. [`SampleQueue::pop_many_full`] then gives the full
    /// timestamps, also after a drop.
    pub fn with_deducer(capacity: usize, first: u64, deducer: TimestampDeducer) -> Self {
        Self::build(capacity, first, Some(deducer))
    }

    fn build(capacity: usize, first: u64, deducer: Option<TimestampDeducer>) -> Self {
        let capacity = capacity.max(1);
        SampleQueue {
            inner: Mutex::new(Ring {
                q: VecDeque::with_capacity(capacity.min(4096)),
                back: first,
                deducer,
                last_popped_ts: None,
                pending_drop_count: 0,
                pending_drop_start: None,
                drop_gaps: VecDeque::new(),
                skips: VecDeque::new(),
            }),
            room: Condvar::new(),
            capacity,
            dropped: AtomicU64::new(0),
            closed: Mutex::new(false),
            woken: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// How many samples the queue holds when full.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many samples the queue has dropped.
    ///
    /// Nothing on the wire carries this number. SPEC.md 8.7 records that a
    /// reader cannot see a loss, so this exists for a local report only.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }

    /// How many samples are waiting.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// True when no sample is waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Add one sample. This never waits.
    ///
    /// A full queue loses its oldest sample. The writer carries on, so a slow
    /// reader never holds back the source. SPEC.md 8.7.
    pub fn push(&self, s: Sample) {
        self.push_packed(PackedSample::from(s));
    }

    /// Add one [`PackedSample`]. This never waits.
    pub fn push_packed(&self, s: PackedSample) {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            q.drop_front();
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
        q.push_back(s);
        q.back = q.back.wrapping_add(1);
        drop(q);
        self.room.notify_one();
    }

    /// Add several samples with one lock. This never waits.
    ///
    /// The result is the same as one [`SampleQueue::push`] for each sample, in
    /// order. A full queue loses its oldest samples, and the count of dropped
    /// samples grows by the number that the queue lost. The recorded behavior in
    /// `artifacts/behavior-liblsl.json` therefore still holds.
    ///
    /// One lock for a block is the difference between a stream that a program
    /// can read at 1000 Hz and one that it cannot.
    pub fn push_many(&self, samples: &[Sample]) {
        if samples.is_empty() {
            return;
        }
        let packed: Vec<PackedSample> = samples.iter().map(PackedSample::from_sample).collect();
        self.push_many_packed(&packed);
    }

    /// Add several [`PackedSample`]s with one lock, cloning only `Arc` handles.
    pub fn push_many_packed(&self, samples: &[PackedSample]) {
        if samples.is_empty() {
            return;
        }
        let mut q = self.inner.lock().unwrap();
        let mut lost = 0u64;
        for s in samples {
            if q.len() >= self.capacity {
                q.drop_front();
                lost += 1;
            }
            q.push_back(s.clone());
        }
        q.back = q.back.wrapping_add(samples.len() as u64);
        drop(q);
        if lost > 0 {
            self.dropped.fetch_add(lost, Ordering::SeqCst);
        }
        if samples.len() == 1 {
            self.room.notify_one();
        } else {
            self.room.notify_all();
        }
    }

    /// Move every sample out of `samples` into the queue, with one lock.
    ///
    /// The result is the same as [`SampleQueue::push_many`], but no sample is
    /// copied. `samples` is empty after the call and keeps its capacity, so
    /// the caller can fill it again with no new allocation.
    ///
    /// A queue with a deducer must not use this call, because the samples
    /// that the queue drops at once never pass the deducer.
    pub fn push_owned(&self, samples: &mut Vec<Sample>) {
        self.push_owned_with_skips(samples, &[]);
    }

    /// Move every [`PackedSample`] out of `samples` into the queue, with one lock.
    pub fn push_owned_packed(&self, samples: &mut Vec<PackedSample>) {
        self.push_owned_packed_with_skips(samples, &[]);
    }

    /// Like [`SampleQueue::push_owned`], and attach sparse `(batch_index, count)`
    /// skipped-sample counts for loss-aware dejitter (`docs/roadmap.md`, step 2).
    pub(crate) fn push_owned_with_skips(&self, samples: &mut Vec<Sample>, skips: &[(usize, u64)]) {
        let mut packed: Vec<PackedSample> = samples.drain(..).map(PackedSample::from).collect();
        self.push_owned_packed_with_skips(&mut packed, skips);
    }

    /// Like [`SampleQueue::push_owned_packed`], and attach sparse
    /// `(batch_index, count)` skipped-sample counts for loss-aware dejitter.
    pub(crate) fn push_owned_packed_with_skips(
        &self,
        samples: &mut Vec<PackedSample>,
        skips: &[(usize, u64)],
    ) {
        if samples.is_empty() {
            if !skips.is_empty() {
                let mut q = self.inner.lock().unwrap();
                let pos = q.back;
                for &(_, count) in skips {
                    q.push_skip(pos, count);
                }
            }
            return;
        }
        let batch_len = samples.len();
        let mut q = self.inner.lock().unwrap();
        debug_assert!(q.deducer.is_none());
        let base_pos = q.back;
        for &(offset, count) in skips {
            q.push_skip(base_pos.wrapping_add(offset as u64), count);
        }
        let mut lost = 0u64;
        // Samples that the queue drops at once are never moved in.
        let skip = batch_len.saturating_sub(self.capacity);
        // Each sample takes a number, also the ones that are never moved in.
        q.back = q.back.wrapping_add(batch_len as u64);
        if skip > 0 {
            let old_len = q.len() as u64;
            if let Some(old) = q.q.front() {
                let ts = old.timestamp;
                q.note_drop(ts, old_len);
                lost += old_len;
                q.q.clear();
            }
            let skipped_ts = samples[0].timestamp;
            q.note_drop(skipped_ts, skip as u64);
            lost += skip as u64;
            let first_kept_pos = base_pos.wrapping_add(skip as u64);
            q.push_skip(first_kept_pos, lost);
            for s in samples.drain(..).skip(skip) {
                q.push_back(s);
            }
        } else {
            for s in samples.drain(..) {
                if q.len() >= self.capacity {
                    q.drop_front();
                    lost += 1;
                }
                q.push_back(s);
            }
        }
        drop(q);
        if lost > 0 {
            self.dropped.fetch_add(lost, Ordering::SeqCst);
        }
        if batch_len == 1 {
            self.room.notify_one();
        } else {
            self.room.notify_all();
        }
    }

    /// Record `count` skipped samples before the next sample that enters this queue.
    pub(crate) fn record_skipped(&self, count: u64) {
        if count == 0 {
            return;
        }
        let mut q = self.inner.lock().unwrap();
        let pos = q.back;
        q.push_skip(pos, count);
    }

    /// Drop any pending skip markers (used when attaching to a new outlet run).
    pub(crate) fn clear_skips(&self) {
        self.inner.lock().unwrap().skips.clear();
    }

    /// Take the oldest sample, waiting up to `timeout`.
    ///
    /// Returns `None` when the timeout passes with nothing waiting, or when the
    /// queue is closed and empty.
    pub fn pop(&self, timeout: Duration) -> Option<Sample> {
        self.pop_packed_with_skip(timeout)
            .map(|(s, _)| s.to_sample())
    }

    /// Take the oldest [`PackedSample`], waiting up to `timeout`.
    pub fn pop_packed(&self, timeout: Duration) -> Option<PackedSample> {
        self.pop_packed_with_skip(timeout).map(|(s, _)| s)
    }

    /// Take the oldest sample and the count of skipped samples immediately
    /// before it, waiting up to `timeout`.
    #[cfg(test)]
    pub(crate) fn pop_with_skip(&self, timeout: Duration) -> Option<(Sample, u64)> {
        self.pop_packed_with_skip(timeout)
            .map(|(s, sk)| (s.to_sample(), sk))
    }

    /// Take the oldest [`PackedSample`] and the count of skipped samples
    /// immediately before it, waiting up to `timeout`.
    pub(crate) fn pop_packed_with_skip(&self, timeout: Duration) -> Option<(PackedSample, u64)> {
        let mut end: Option<Instant> = None;
        let mut q = self.inner.lock().unwrap();
        loop {
            if !q.is_empty() {
                let pos = q.front_number();
                let skipped = q.take_skips_at(pos);
                let s = q.pop_front().unwrap();
                q.note_pop(s.timestamp, s.timestamp);
                return Some((s, skipped));
            }
            if timeout.is_zero() || *self.closed.lock().unwrap() {
                return None;
            }
            let deadline = *end.get_or_insert_with(|| Instant::now() + timeout);
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let (guard, result) = self
                .room
                .wait_timeout(q, left.min(Duration::from_millis(100)))
                .unwrap();
            q = guard;
            if result.timed_out() && Instant::now() >= deadline {
                if !q.is_empty() {
                    let pos = q.front_number();
                    let skipped = q.take_skips_at(pos);
                    let s = q.pop_front().unwrap();
                    q.note_pop(s.timestamp, s.timestamp);
                    return Some((s, skipped));
                }
                return None;
            }
        }
    }

    /// Take every sample that waits, up to `max`, with one lock.
    ///
    /// The samples go to the end of `out`. The call gives how many it added.
    ///
    /// The timeout applies to the first sample only. When one sample waits, the
    /// call takes what is there and returns. It never waits for `max` samples,
    /// because a program that shows live signals must show what arrived.
    ///
    /// A closed and empty queue gives zero.
    pub fn pop_many(&self, out: &mut Vec<Sample>, max: usize, timeout: Duration) -> usize {
        self.pop_many_with_skips(out, None, max, timeout)
    }

    /// Take every [`PackedSample`] that waits, up to `max`, with one lock.
    pub fn pop_many_packed(
        &self,
        out: &mut Vec<PackedSample>,
        max: usize,
        timeout: Duration,
    ) -> usize {
        self.pop_many_packed_with_skips(out, None, max, timeout)
    }

    /// Like [`SampleQueue::pop_many`], and append `(offset_in_popped, skipped)`
    /// pairs to `skips_out` when any samples were skipped within this block.
    pub(crate) fn pop_many_with_skips(
        &self,
        out: &mut Vec<Sample>,
        skips_out: Option<&mut Vec<(usize, u64)>>,
        max: usize,
        timeout: Duration,
    ) -> usize {
        let mut packed = Vec::new();
        let got = self.pop_many_packed_with_skips(&mut packed, skips_out, max, timeout);
        out.extend(packed.iter().map(PackedSample::to_sample));
        got
    }

    /// Like [`SampleQueue::pop_many_packed`], and append `(offset_in_popped, skipped)`
    /// pairs to `skips_out` when any samples were skipped within this block.
    pub(crate) fn pop_many_packed_with_skips(
        &self,
        out: &mut Vec<PackedSample>,
        mut skips_out: Option<&mut Vec<(usize, u64)>>,
        max: usize,
        timeout: Duration,
    ) -> usize {
        if max == 0 {
            return 0;
        }
        let mut end: Option<Instant> = None;
        let mut q = self.inner.lock().unwrap();
        let drain_into = |q: &mut Ring,
                          out: &mut Vec<PackedSample>,
                          skips_out: &mut Option<&mut Vec<(usize, u64)>>,
                          max: usize|
         -> usize {
            let take = q.len().min(max);
            if take == 0 {
                return 0;
            }
            let first_pos = q.front_number();
            if !q.skips.is_empty() {
                if let Some(dst) = skips_out.as_mut() {
                    let lead = q.take_skips_at(first_pos);
                    if lead > 0 {
                        dst.push((0, lead));
                    }
                    let end_pos = first_pos.wrapping_add(take as u64);
                    while let Some(&(pos, count)) = q.skips.front() {
                        if pos < end_pos {
                            let idx = (pos.wrapping_sub(first_pos)) as usize;
                            dst.push((idx, count));
                            q.skips.pop_front();
                        } else {
                            break;
                        }
                    }
                } else {
                    let end_pos = first_pos.wrapping_add(take as u64);
                    while let Some(&(pos, _)) = q.skips.front() {
                        if pos < end_pos {
                            q.skips.pop_front();
                        } else {
                            break;
                        }
                    }
                }
            }
            let first_ts = q.q[0].timestamp;
            let last_ts = q.q[take - 1].timestamp;
            out.extend(q.drain(..take));
            q.note_pop(first_ts, last_ts);
            take
        };

        loop {
            if !q.is_empty() {
                return drain_into(&mut q, out, &mut skips_out, max);
            }
            if timeout.is_zero() || *self.closed.lock().unwrap() {
                return 0;
            }
            let deadline = *end.get_or_insert_with(|| Instant::now() + timeout);
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return 0;
            }
            let (guard, result) = self
                .room
                .wait_timeout(q, left.min(Duration::from_millis(100)))
                .unwrap();
            q = guard;
            if result.timed_out() && Instant::now() >= deadline {
                return drain_into(&mut q, out, &mut skips_out, max);
            }
        }
    }

    /// Like [`SampleQueue::pop_many`], and also give the number of the first
    /// sample that it took.
    ///
    /// The samples that it took have consecutive numbers. With no sample, the
    /// number is the one that the next sample gets.
    pub fn pop_many_numbered(
        &self,
        out: &mut Vec<Sample>,
        max: usize,
        timeout: Duration,
    ) -> (usize, u64) {
        let mut packed = Vec::new();
        let res = self.pop_many_packed_inner(&mut packed, None, max, timeout);
        out.extend(packed.iter().map(PackedSample::to_sample));
        res
    }

    /// Like [`SampleQueue::pop_many_numbered`], returning [`PackedSample`]s.
    pub fn pop_many_numbered_packed(
        &self,
        out: &mut Vec<PackedSample>,
        max: usize,
        timeout: Duration,
    ) -> (usize, u64) {
        self.pop_many_packed_inner(out, None, max, timeout)
    }

    /// Like [`SampleQueue::pop_many_numbered`], and also add the full
    /// timestamp of each sample that it took to the end of `full`.
    ///
    /// A queue with no deducer gives the timestamps of the samples as they
    /// are.
    pub fn pop_many_full(
        &self,
        out: &mut Vec<Sample>,
        full: &mut Vec<f64>,
        max: usize,
        timeout: Duration,
    ) -> (usize, u64) {
        let mut packed = Vec::new();
        let res = self.pop_many_packed_inner(&mut packed, Some(full), max, timeout);
        out.extend(packed.iter().map(PackedSample::to_sample));
        res
    }

    /// Like [`SampleQueue::pop_many_full`], returning [`PackedSample`]s.
    pub fn pop_many_full_packed(
        &self,
        out: &mut Vec<PackedSample>,
        full: &mut Vec<f64>,
        max: usize,
        timeout: Duration,
    ) -> (usize, u64) {
        self.pop_many_packed_inner(out, Some(full), max, timeout)
    }

    fn pop_many_packed_inner(
        &self,
        out: &mut Vec<PackedSample>,
        mut full: Option<&mut Vec<f64>>,
        max: usize,
        timeout: Duration,
    ) -> (usize, u64) {
        let mut end: Option<Instant> = None;
        let mut q = self.inner.lock().unwrap();
        loop {
            if max == 0
                || !q.is_empty()
                || timeout.is_zero()
                || *self.closed.lock().unwrap()
                || self.woken.swap(false, Ordering::SeqCst)
            {
                let first = q.front_number();
                let take = q.len().min(max);
                let ring = &mut *q;
                for s in ring.q.drain(..take) {
                    // The deducer sees each sample that leaves the queue, also
                    // when the caller does not ask for the full timestamps.
                    let t = match ring.deducer.as_mut() {
                        Some(d) => d.apply(s.timestamp),
                        None => s.timestamp,
                    };
                    if let Some(f) = full.as_mut() {
                        f.push(t);
                    }
                    out.push(s);
                }
                return (take, first);
            }
            let deadline = *end.get_or_insert_with(|| Instant::now() + timeout);
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return (0, q.front_number());
            }
            let (guard, _) = self
                .room
                .wait_timeout(q, left.min(Duration::from_millis(100)))
                .unwrap();
            q = guard;
        }
    }

    /// End the current wait of [`SampleQueue::pop_many_numbered`] or
    /// [`SampleQueue::pop_many_full`] at once, with what the queue holds. A
    /// session uses it when a request of its consumer arrives.
    pub fn wake(&self) {
        let _q = self.inner.lock().unwrap();
        self.woken.store(true, Ordering::SeqCst);
        self.room.notify_all();
    }

    /// Wake every reader and stop the queue.
    pub fn close(&self) {
        *self.closed.lock().unwrap() = true;
        self.room.notify_all();
    }

    /// True once [`SampleQueue::close`] has run.
    pub fn is_closed(&self) -> bool {
        *self.closed.lock().unwrap()
    }

    /// Number of recorded drop gaps (including any in-progress overflow).
    pub(crate) fn drop_gap_count(&self) -> usize {
        let q = self.inner.lock().unwrap();
        let pending = usize::from(q.pending_drop_count > 0 && q.q.front().is_some());
        q.drop_gaps.len() + pending
    }

    /// Snapshot all drop gaps on this queue, shifted by `shift`.
    pub(crate) fn drop_gaps(&self, shift: f64) -> Vec<Gap> {
        let q = self.inner.lock().unwrap();
        let mut out: Vec<Gap> = q
            .drop_gaps
            .iter()
            .map(|g| Gap {
                start: g.start + shift,
                end: g.end + shift,
                count: g.count,
                kind: g.kind,
            })
            .collect();
        if q.pending_drop_count > 0 {
            if let Some(front) = q.q.front() {
                let start = q.pending_drop_start.unwrap_or(front.timestamp);
                if start.is_finite() && front.timestamp.is_finite() {
                    out.push(Gap {
                        start: start + shift,
                        end: front.timestamp + shift,
                        count: q.pending_drop_count,
                        kind: GapKind::Dropped,
                    });
                }
            }
        }
        out
    }

    /// Drain all finalized drop gaps on this queue, shifted by `shift`.
    pub(crate) fn take_drop_gaps(&self, shift: f64) -> Vec<Gap> {
        let mut q = self.inner.lock().unwrap();
        q.drop_gaps
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

#[derive(Debug)]
struct ConsumerEntry {
    id: u64,
    peer: String,
    queue: Arc<SampleQueue>,
}

/// A set of consumers that all receive the same sample.
///
/// liblsl calls this the send buffer (`src/send_buffer.h`). Each consumer holds
/// its own queue, so a slow consumer loses only its own samples. SPEC.md 8.8.
///
/// The fanout also holds the history of the `resume` feature, and the
/// timestamp deducer that gives the history its full timestamps.
/// `docs/extensions.md`, section `resume`.
#[derive(Debug, Default)]
pub struct Fanout {
    consumers: Mutex<Consumers>,
}

/// The queues of a fanout, the number of the next pushed sample, and the
/// history.
///
/// One lock holds all of them, so a new queue starts at the number that the
/// next push gives, every queue sees the same numbers, and the history holds
/// the same numbers.
#[derive(Debug)]
struct Consumers {
    list: Vec<ConsumerEntry>,
    next_consumer_id: u64,
    next: u64,
    /// Calculates the full timestamp of each pushed sample, in the order of
    /// the numbers. It runs also when there is no history, so that a history
    /// that starts in the middle of a chunk gets correct timestamps.
    deducer: TimestampDeducer,
    srate: f64,
    limits: HistoryLimits,
    /// `None` until the first consumer agrees to `resume/1`.
    history: Option<History>,
}

impl Default for Consumers {
    fn default() -> Self {
        Consumers {
            list: Vec::new(),
            next_consumer_id: 1,
            next: 0,
            deducer: TimestampDeducer::new(IRREGULAR_RATE),
            srate: IRREGULAR_RATE,
            limits: HistoryLimits::DEFAULT,
            history: None,
        }
    }
}

impl Consumers {
    /// Give one sample its number, its full timestamp, and its place in the
    /// history.
    fn record_packed(&mut self, s: &PackedSample) {
        let full = self.deducer.apply(s.timestamp);
        if let Some(h) = self.history.as_mut() {
            debug_assert_eq!(h.next(), self.next);
            h.push_packed(s.with_timestamp(full));
        }
        self.next = self.next.wrapping_add(1);
    }
}

impl Fanout {
    /// Build an empty set for a stream with an irregular rate and the
    /// default history limits.
    pub fn new() -> Self {
        Fanout::default()
    }

    /// Build an empty set for a stream with the nominal rate `srate`.
    ///
    /// The rate gives the deduced timestamps and the time limit of the
    /// history.
    pub fn with_rate(srate: f64, limits: HistoryLimits) -> Self {
        Fanout {
            consumers: Mutex::new(Consumers {
                deducer: TimestampDeducer::new(srate),
                srate,
                limits,
                ..Consumers::default()
            }),
        }
    }

    /// Start the history, if the limits allow it. The result is true when
    /// the history is on.
    ///
    /// The history holds each sample that the outlet pushes from this point.
    /// A second call changes nothing. `docs/extensions.md`, history rule 3.
    pub fn start_history(&self) -> bool {
        let mut set = self.consumers.lock().unwrap();
        if set.history.is_none() && set.limits.allow_history() {
            let h = History::new(set.limits.clone(), set.srate, set.next);
            set.history = Some(h);
        }
        set.history.is_some()
    }

    /// How many samples the history holds across RAM and disk. Zero when it is off.
    pub fn history_len(&self) -> usize {
        let set = self.consumers.lock().unwrap();
        set.history.as_ref().map_or(0, History::len)
    }

    /// Bytes currently held by the history in RAM. Zero when it is off.
    pub fn history_ram_bytes(&self) -> usize {
        let set = self.consumers.lock().unwrap();
        set.history.as_ref().map_or(0, History::bytes)
    }

    /// Bytes currently held by the history in disk spool segments. Zero when off.
    pub fn history_disk_bytes(&self) -> u64 {
        let set = self.consumers.lock().unwrap();
        set.history.as_ref().map_or(0, History::disk_bytes)
    }

    /// Path of the active disk spool directory, if disk spilling has started.
    pub fn history_spool_dir(&self) -> Option<std::path::PathBuf> {
        let set = self.consumers.lock().unwrap();
        set.history.as_ref().and_then(History::spool_dir)
    }

    /// Copy at most `max` samples of the history to the end of `out`, from
    /// the number `from`, and below the number `until`. Each sample has its
    /// full timestamp.
    ///
    /// The call takes the lock of the set one time. A push thus waits for at
    /// most the copy of `max` samples (and never for disk I/O).
    /// [`History::read`] gives the rules and the result.
    pub fn read_history(
        &self,
        from: u64,
        until: u64,
        max: usize,
        out: &mut Vec<Sample>,
    ) -> Option<(u64, usize)> {
        let mut packed = Vec::new();
        let res = self.read_history_packed(from, until, max, &mut packed)?;
        out.extend(packed.iter().map(PackedSample::to_sample));
        Some(res)
    }

    /// Share at most `max` packed samples of the history to the end of `out`.
    ///
    /// RAM reads clone `Arc` handles under the lock; disk reads execute
    /// outside the `Fanout` lock so live pushes and other consumers never
    /// wait on disk I/O.
    pub fn read_history_packed(
        &self,
        from: u64,
        until: u64,
        max: usize,
        out: &mut Vec<PackedSample>,
    ) -> Option<(u64, usize)> {
        let plan = {
            let set = self.consumers.lock().unwrap();
            set.history
                .as_ref()?
                .plan_read_packed(from, until, max, out)
        };
        match plan {
            HistoryRead::Empty => None,
            HistoryRead::Ready { first, count } => Some((first, count)),
            HistoryRead::Disk(disk_plan) => disk_plan.execute(out),
        }
    }

    /// Add a consumer with its own queue.
    pub fn add(&self, capacity: usize) -> Arc<SampleQueue> {
        self.add_with_peer(capacity, String::new())
    }

    /// Add a consumer with its own queue and peer label.
    pub(crate) fn add_with_peer(&self, capacity: usize, peer: String) -> Arc<SampleQueue> {
        let mut set = self.consumers.lock().unwrap();
        let id = set.next_consumer_id;
        set.next_consumer_id = id.wrapping_add(1);
        let q = Arc::new(SampleQueue::with_first_number(capacity, set.next));
        set.list.push(ConsumerEntry {
            id,
            peer,
            queue: Arc::clone(&q),
        });
        q
    }

    /// Add a consumer for the `resume` feature. The result is its queue and
    /// `N`, the number of the first sample that the queue gets.
    ///
    /// The queue gives the full timestamp of each sample
    /// ([`SampleQueue::pop_many_full`]). The history holds the samples below
    /// `N`. `docs/extensions.md`, resend rule 1.
    pub fn add_resumable(&self, capacity: usize) -> (Arc<SampleQueue>, u64) {
        self.add_resumable_with_peer(capacity, String::new())
    }

    /// Add a resumable consumer with its peer label.
    pub(crate) fn add_resumable_with_peer(
        &self,
        capacity: usize,
        peer: String,
    ) -> (Arc<SampleQueue>, u64) {
        let mut set = self.consumers.lock().unwrap();
        let id = set.next_consumer_id;
        set.next_consumer_id = id.wrapping_add(1);
        let n = set.next;
        let q = Arc::new(SampleQueue::with_deducer(capacity, n, set.deducer.clone()));
        set.list.push(ConsumerEntry {
            id,
            peer,
            queue: Arc::clone(&q),
        });
        (q, n)
    }

    /// The number that the next pushed sample gets.
    pub fn next_number(&self) -> u64 {
        self.consumers.lock().unwrap().next
    }

    /// Remove a consumer.
    pub fn remove(&self, q: &Arc<SampleQueue>) {
        self.consumers
            .lock()
            .unwrap()
            .list
            .retain(|c| !Arc::ptr_eq(&c.queue, q));
    }

    /// How many consumers are connected.
    pub fn count(&self) -> usize {
        self.consumers.lock().unwrap().list.len()
    }

    /// Snapshot the fill and drop state of every connected consumer queue.
    pub fn health(&self) -> OutletHealth {
        let mut set = self.consumers.lock().unwrap();
        set.list.retain(|c| !c.queue.is_closed());
        let consumers = set
            .list
            .iter()
            .map(|c| ConsumerHealth {
                id: c.id,
                peer: c.peer.clone(),
                fill: c.queue.len(),
                capacity: c.queue.capacity(),
                dropped: c.queue.dropped(),
            })
            .collect();
        OutletHealth {
            consumers,
            history_len: set.history.as_ref().map_or(0, History::len),
            pushed: set.next,
        }
    }

    /// Give one sample to every consumer.
    ///
    /// A consumer that has gone away is dropped from the set. The call never
    /// waits, whatever any consumer is doing.
    pub fn push(&self, s: &Sample) {
        self.push_packed(PackedSample::from_sample(s));
    }

    /// Give one [`PackedSample`] to every consumer and the `resume` history,
    /// sharing its `Arc` payload with zero byte copies.
    pub fn push_packed(&self, s: PackedSample) {
        let mut set = self.consumers.lock().unwrap();
        // The consumers get the sample first, so that recording into the
        // history does not delay them. The lock holds both steps, so a new
        // queue and the history still agree on each number.
        set.list.retain(|c| !c.queue.is_closed());
        for c in set.list.iter() {
            c.queue.push_packed(s.clone());
        }
        set.record_packed(&s);
    }

    /// Give several samples to every consumer, with one lock for the set.
    ///
    /// The result for each consumer is the same as one [`Fanout::push`] for each
    /// sample, in order.
    pub fn push_many(&self, samples: &[Sample]) {
        if samples.is_empty() {
            return;
        }
        let packed: Vec<PackedSample> = samples.iter().map(PackedSample::from_sample).collect();
        self.push_many_packed(&packed);
    }

    /// Give several [`PackedSample`]s to every consumer and the `resume`
    /// history with one lock, sharing each `Arc` payload with zero byte copies.
    pub fn push_many_packed(&self, samples: &[PackedSample]) {
        if samples.is_empty() {
            return;
        }
        let mut set = self.consumers.lock().unwrap();
        // The consumers first, as in [`Fanout::push_packed`].
        set.list.retain(|c| !c.queue.is_closed());
        for c in set.list.iter() {
            c.queue.push_many_packed(samples);
        }
        for s in samples {
            set.record_packed(s);
        }
    }

    /// Close every consumer queue.
    pub fn close(&self) {
        for c in self.consumers.lock().unwrap().list.iter() {
            c.queue.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use labstream_wire::Value;

    fn sample(i: i32) -> Sample {
        Sample {
            timestamp: i as f64,
            values: vec![Value::I32(i)],
        }
    }

    fn index(s: &Sample) -> i32 {
        match s.values[0] {
            Value::I32(v) => v,
            _ => panic!("wrong format"),
        }
    }

    #[test]
    fn a_full_queue_keeps_the_newest() {
        // The recorded shape from `artifacts/behavior-liblsl.json`, in small.
        let q = SampleQueue::new(100);
        for i in 0..600 {
            q.push(sample(i));
        }
        assert_eq!(q.len(), 100);
        assert_eq!(q.dropped(), 500);

        let first = q.pop(Duration::from_millis(10)).expect("a sample");
        assert_eq!(index(&first), 500, "the oldest surviving sample");

        let mut last = first;
        while let Some(s) = q.pop(Duration::from_millis(10)) {
            last = s;
        }
        assert_eq!(index(&last), 599, "the newest sample");
    }

    #[test]
    fn a_block_write_gives_what_single_writes_give() {
        // The batch call is an optimization. It must lose the same samples, keep
        // the same samples, and count the same drops.
        let one = SampleQueue::new(100);
        let many = SampleQueue::new(100);
        let block: Vec<Sample> = (0..600).map(sample).collect();
        for s in &block {
            one.push(s.clone());
        }
        many.push_many(&block);

        assert_eq!(one.len(), many.len());
        assert_eq!(one.dropped(), many.dropped());
        while let Some(a) = one.pop(Duration::from_millis(10)) {
            let b = many.pop(Duration::from_millis(10)).expect("the same count");
            assert_eq!(index(&a), index(&b));
        }
        assert!(many.pop(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn an_owned_block_write_gives_what_single_writes_give() {
        // The same rules as `push_many`, with the existing samples first, then
        // a block larger than the queue.
        for (before, block_len) in [(0, 50), (30, 50), (30, 600), (0, 100), (99, 1)] {
            let one = SampleQueue::new(100);
            let owned = SampleQueue::new(100);
            for i in 0..before {
                one.push(sample(i));
                owned.push(sample(i));
            }
            let mut block: Vec<Sample> = (1000..1000 + block_len).map(sample).collect();
            for s in &block {
                one.push(s.clone());
            }
            owned.push_owned(&mut block);
            assert!(block.is_empty(), "the call takes every sample");

            assert_eq!(one.len(), owned.len());
            assert_eq!(one.dropped(), owned.dropped(), "{before} then {block_len}");
            while let Some(a) = one.pop(Duration::from_millis(10)) {
                let b = owned
                    .pop(Duration::from_millis(10))
                    .expect("the same count");
                assert_eq!(index(&a), index(&b));
            }
            assert!(owned.pop(Duration::from_millis(10)).is_none());
        }
    }

    #[test]
    fn a_block_read_takes_what_waits_in_order() {
        let q = SampleQueue::new(100);
        q.push_many(&(0..10).map(sample).collect::<Vec<_>>());

        let mut out = Vec::new();
        let got = q.pop_many(&mut out, 4, Duration::from_millis(10));
        assert_eq!(got, 4, "the call stops at max");
        assert_eq!(out.iter().map(index).collect::<Vec<_>>(), [0, 1, 2, 3]);

        let got = q.pop_many(&mut out, 100, Duration::from_millis(10));
        assert_eq!(got, 6, "the call takes what waits, and not more");
        assert_eq!(out.len(), 10, "the samples go to the end of the buffer");
        assert_eq!(index(&out[9]), 9);
    }

    #[test]
    fn a_block_read_of_an_empty_queue_gives_nothing() {
        let q = SampleQueue::new(10);
        let mut out = Vec::new();
        assert_eq!(q.pop_many(&mut out, 8, Duration::from_millis(5)), 0);
        assert!(out.is_empty());

        q.close();
        assert_eq!(q.pop_many(&mut out, 8, Duration::from_millis(50)), 0);
    }

    #[test]
    fn a_block_read_waits_for_the_first_sample_only() {
        // A reader that waited for `max` samples would hold a viewer back by the
        // time that the rest of the block needs.
        let q = Arc::new(SampleQueue::new(10));
        let writer = Arc::clone(&q);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            writer.push_many(&[sample(1), sample(2)]);
        });
        let mut out = Vec::new();
        let start = Instant::now();
        let got = q.pop_many(&mut out, 1000, Duration::from_secs(5));
        assert_eq!(got, 2, "the call takes the block that arrived");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the call returned"
        );
    }

    #[test]
    fn a_block_reaches_every_consumer() {
        let fan = Fanout::new();
        let a = fan.add(10);
        let b = fan.add(10);
        fan.push_many(&[sample(7), sample(8)]);

        for q in [&a, &b] {
            let mut out = Vec::new();
            assert_eq!(q.pop_many(&mut out, 10, Duration::from_millis(10)), 2);
            assert_eq!(out.iter().map(index).collect::<Vec<_>>(), [7, 8]);
        }
    }

    #[test]
    fn the_writer_never_waits() {
        // A queue of one, written a million times, must finish at once. A
        // design that waited for a reader would never return.
        let q = SampleQueue::new(1);
        let start = Instant::now();
        for i in 0..1_000_000 {
            q.push(sample(i));
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the writer waited"
        );
        assert_eq!(q.len(), 1);
        assert_eq!(index(&q.pop(Duration::from_millis(10)).unwrap()), 999_999);
    }

    #[test]
    fn a_capacity_of_zero_floors_at_one() {
        let q = SampleQueue::new(0);
        assert_eq!(q.capacity(), 1);
        q.push(sample(7));
        assert_eq!(index(&q.pop(Duration::from_millis(10)).unwrap()), 7);
    }

    #[test]
    fn an_empty_queue_reports_nothing_after_the_timeout() {
        let q = SampleQueue::new(4);
        let start = Instant::now();
        assert!(q.pop(Duration::from_millis(120)).is_none());
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn a_closed_queue_stops_a_reader_at_once() {
        let q = Arc::new(SampleQueue::new(4));
        let c = Arc::clone(&q);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            c.close();
        });
        let start = Instant::now();
        assert!(q.pop(Duration::from_secs(10)).is_none());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the reader waited out the timeout"
        );
    }

    #[test]
    fn a_reader_wakes_when_a_sample_arrives() {
        let q = Arc::new(SampleQueue::new(4));
        let c = Arc::clone(&q);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            c.push(sample(42));
        });
        let got = q.pop(Duration::from_secs(2)).expect("a sample");
        assert_eq!(index(&got), 42);
    }

    #[test]
    fn a_slow_consumer_never_holds_back_a_fast_one() {
        // SPEC.md 8.8. The fast queue is large and the slow queue is small.
        let f = Fanout::new();
        let fast = f.add(1000);
        let slow = f.add(10);
        for i in 0..500 {
            f.push(&sample(i));
        }
        assert_eq!(fast.len(), 500, "the fast consumer lost samples");
        assert_eq!(fast.dropped(), 0);
        assert_eq!(slow.len(), 10);
        assert_eq!(slow.dropped(), 490);
    }

    #[test]
    fn a_closed_consumer_leaves_the_set() {
        let f = Fanout::new();
        let a = f.add(10);
        let b = f.add(10);
        assert_eq!(f.count(), 2);
        b.close();
        f.push(&sample(1));
        assert_eq!(f.count(), 1, "a closed consumer must be removed");
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn removing_a_consumer_stops_its_samples() {
        let f = Fanout::new();
        let a = f.add(10);
        f.push(&sample(1));
        f.remove(&a);
        f.push(&sample(2));
        assert_eq!(a.len(), 1, "a removed consumer must receive nothing more");
    }

    /// A small generator, so the test runs the same steps on each run.
    struct Steps(u64);

    impl Steps {
        fn next(&mut self, below: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % below
        }
    }

    #[test]
    fn the_numbers_follow_a_model_with_one_number_for_each_sample() {
        // Each sample carries its own number as its value. The model keeps
        // the numbers. Every way to push, drop and pop must agree with it.
        for seed in 1..40u64 {
            let mut steps = Steps(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let capacity = 1 + steps.next(12) as usize;
            let first = steps.next(1000);
            let q = SampleQueue::with_first_number(capacity, first);
            let mut model: VecDeque<u64> = VecDeque::new();
            let mut next = first;
            for _ in 0..400 {
                match steps.next(5) {
                    0 => {
                        q.push(sample(next as i32));
                        model.push_back(next);
                        next += 1;
                    }
                    1 | 2 => {
                        let k = steps.next(30) as usize;
                        let block: Vec<Sample> =
                            (0..k).map(|i| sample((next + i as u64) as i32)).collect();
                        if steps.next(2) == 0 {
                            q.push_many(&block);
                        } else {
                            let mut owned = block;
                            q.push_owned(&mut owned);
                        }
                        for i in 0..k as u64 {
                            model.push_back(next + i);
                        }
                        next += k as u64;
                    }
                    3 => {
                        let got = q.pop(Duration::ZERO);
                        let want = model.pop_front();
                        assert_eq!(got.map(|s| index(&s) as u64), want);
                    }
                    _ => {
                        let max = 1 + steps.next(20) as usize;
                        let mut out = Vec::new();
                        let (n, number) = q.pop_many_numbered(&mut out, max, Duration::ZERO);
                        assert_eq!(n, out.len());
                        let want_first = model.front().copied().unwrap_or(next);
                        assert_eq!(number, want_first, "seed {seed}");
                        for (i, s) in out.iter().enumerate() {
                            assert_eq!(index(s) as u64, number + i as u64, "seed {seed}");
                            assert_eq!(model.pop_front(), Some(number + i as u64));
                        }
                    }
                }
                while model.len() > capacity {
                    model.pop_front();
                }
                assert_eq!(q.len(), model.len(), "seed {seed}");
            }
        }
    }

    #[test]
    fn a_late_consumer_starts_at_the_current_number() {
        let f = Fanout::new();
        let a = f.add(100);
        for i in 0..5 {
            f.push(&sample(i));
        }
        let b = f.add(100);
        assert_eq!(f.next_number(), 5);
        f.push_many(&[sample(5), sample(6)]);
        let mut out = Vec::new();
        assert_eq!(a.pop_many_numbered(&mut out, 100, Duration::ZERO), (7, 0));
        out.clear();
        assert_eq!(b.pop_many_numbered(&mut out, 100, Duration::ZERO), (2, 5));
        assert_eq!(index(&out[0]), 5);
    }

    /// A chunk as `Outlet::push_chunk` makes it: a timestamp on the first
    /// sample, and the deduced tag on the others. SPEC.md 9.2.
    fn chunk(first: i32, len: i32, t: f64) -> Vec<Sample> {
        (first..first + len)
            .map(|i| Sample {
                timestamp: if i == first {
                    t
                } else {
                    labstream_wire::DEDUCED_TIMESTAMP
                },
                values: vec![Value::I32(i)],
            })
            .collect()
    }

    #[test]
    fn the_history_is_off_until_a_consumer_asks_for_it() {
        let f = Fanout::with_rate(100.0, HistoryLimits::DEFAULT);
        let q = f.add(100);
        f.push_many(&chunk(0, 10, 1.0));
        assert_eq!(f.history_len(), 0);
        assert_eq!(f.read_history(0, 10, 256, &mut Vec::new()), None);

        assert!(f.start_history());
        assert!(f.start_history(), "a second call changes nothing");
        assert_eq!(
            f.history_len(),
            0,
            "the history holds no sample from before"
        );

        // The history holds the samples that no consumer gets.
        f.remove(&q);
        f.push_many(&chunk(10, 5, 2.0));
        f.push(&sample(15));
        assert_eq!(f.history_len(), 6);
        let mut out = Vec::new();
        assert_eq!(
            f.read_history(0, f.next_number(), 256, &mut out),
            Some((10, 6))
        );
        assert_eq!(
            out.iter().map(index).collect::<Vec<_>>(),
            (10..16).collect::<Vec<_>>()
        );
    }

    #[test]
    fn limits_that_turn_the_history_off_keep_it_off() {
        let f = Fanout::with_rate(100.0, HistoryLimits::OFF);
        assert!(!f.start_history());
        f.push(&sample(0));
        assert_eq!(f.history_len(), 0);
    }

    #[test]
    fn the_history_and_the_queues_use_the_same_numbers() {
        let f = Fanout::with_rate(100.0, HistoryLimits::DEFAULT);
        f.push_many(&chunk(0, 7, 1.0));
        f.start_history();
        let q = f.add(100);
        f.push_many(&chunk(7, 20, 2.0));
        let mut live = Vec::new();
        let (_, first) = q.pop_many_numbered(&mut live, 100, Duration::ZERO);
        assert_eq!(first, 7);
        let mut old = Vec::new();
        assert_eq!(
            f.read_history(first, u64::MAX / 4, 256, &mut old),
            Some((7, 20))
        );
        let a: Vec<i32> = live.iter().map(index).collect();
        let b: Vec<i32> = old.iter().map(index).collect();
        assert_eq!(a, b);
        // A resend stops below the first number of the new queue, so no
        // sample goes to the consumer twice.
        assert_eq!(f.read_history(0, first, 256, &mut Vec::new()), None);
        let q2 = f.add(100);
        let n = f.next_number();
        f.push_many(&chunk(27, 5, 3.0));
        let mut resent = Vec::new();
        assert_eq!(f.read_history(10, n, 256, &mut resent), Some((10, 17)));
        assert_eq!(index(resent.last().unwrap()) as u64, n - 1);
        let mut live = Vec::new();
        assert_eq!(q2.pop_many_numbered(&mut live, 100, Duration::ZERO), (5, n));
    }

    #[test]
    fn the_history_holds_the_timestamps_that_an_inlet_calculates() {
        // A rate with no exact binary form, so that each addition rounds.
        let srate = 300.0;
        let f = Fanout::with_rate(srate, HistoryLimits::DEFAULT);
        // Push the first part of a chunk, then start the history in the
        // middle of the next chunk.
        let mut pushed = chunk(0, 9, 1.234_567);
        pushed.extend(chunk(9, 9, 1.5));
        let second = chunk(18, 9, 2.0);
        f.push_many(&pushed);
        f.push_many(&second[..4]);
        f.start_history();
        f.push_many(&second[4..]);
        f.push(&sample(27));
        pushed.extend(second);
        pushed.push(sample(27));

        // An inlet with no dropout reads every sample of the connection.
        let mut inlet = TimestampDeducer::new(srate);
        let want: Vec<u64> = pushed
            .iter()
            .map(|s| inlet.apply(s.timestamp).to_bits())
            .collect();

        let mut got = Vec::new();
        assert_eq!(f.read_history(0, 28, 256, &mut got), Some((22, 6)));
        assert_eq!(got.len(), 6);
        for s in &got {
            assert_ne!(s.timestamp, labstream_wire::DEDUCED_TIMESTAMP);
            let i = index(s) as usize;
            assert_eq!(s.timestamp.to_bits(), want[i], "sample {i}");
        }
    }

    #[test]
    fn a_read_takes_one_block() {
        let f = Fanout::with_rate(1000.0, HistoryLimits::DEFAULT);
        f.start_history();
        for i in 0..1000 {
            f.push(&sample(i));
        }
        let mut out = Vec::new();
        let mut from = 0;
        let mut blocks = 0;
        while let Some((first, count)) = f.read_history(from, 1000, 256, &mut out) {
            from = first + count as u64;
            blocks += 1;
        }
        assert_eq!(blocks, 4);
        assert_eq!(out.len(), 1000);
        assert_eq!(
            out.iter().map(index).collect::<Vec<_>>(),
            (0..1000).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_full_queue_records_a_coalesced_drop_gap() {
        let q = SampleQueue::new(4);
        q.push(sample(10));
        let first = q.pop(Duration::from_millis(10)).unwrap();
        assert_eq!(first.timestamp, 10.0);

        // Overflow the 4-sample queue with 10 samples (11..=20): drops 6 (11..=16).
        let mut batch: Vec<Sample> = (11..=20).map(sample).collect();
        q.push_owned(&mut batch);
        assert_eq!(q.dropped(), 6);
        let pending = q.drop_gaps(0.0);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].start, 10.0);
        assert_eq!(pending[0].end, 17.0);
        assert_eq!(pending[0].count, 6);
        assert_eq!(pending[0].kind, GapKind::Dropped);

        let next = q.pop(Duration::from_millis(10)).unwrap();
        assert_eq!(next.timestamp, 17.0);
        let taken = q.take_drop_gaps(50.0);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].start, 60.0);
        assert_eq!(taken[0].end, 67.0);
        assert_eq!(taken[0].count, 6);
        assert!(q.drop_gaps(0.0).is_empty());
    }

    #[test]
    fn fanout_health_reports_fill_capacity_and_drops_per_consumer() {
        let fan = Fanout::new();
        let fast = fan.add_with_peer(10, "127.0.0.1:40001".to_string());
        let slow = fan.add_with_peer(3, "127.0.0.1:40002".to_string());

        for i in 0..8 {
            fan.push(&sample(i));
            let _ = fast.pop(Duration::from_millis(1));
        }

        let h = fan.health();
        assert_eq!(h.pushed, 8);
        assert_eq!(h.consumers.len(), 2);
        assert_eq!(h.consumers[0].id, 1);
        assert_eq!(h.consumers[0].peer, "127.0.0.1:40001");
        assert_eq!(h.consumers[0].fill, 0);
        assert_eq!(h.consumers[0].capacity, 10);
        assert_eq!(h.consumers[0].dropped, 0);

        assert_eq!(h.consumers[1].id, 2);
        assert_eq!(h.consumers[1].peer, "127.0.0.1:40002");
        assert_eq!(h.consumers[1].fill, 3);
        assert_eq!(h.consumers[1].capacity, 3);
        assert_eq!(h.consumers[1].dropped, 5);
        drop(slow);
    }

    #[test]
    fn queue_delivers_exact_skip_positions_on_pop_and_pop_many() {
        let q = SampleQueue::new(32);
        let mut batch: Vec<Sample> = (0..6).map(sample).collect();
        // 100 samples lost between sample 2 and sample 3.
        q.push_owned_with_skips(&mut batch, &[(3, 100)]);

        let mut out = Vec::new();
        let mut skips = Vec::new();
        let got = q.pop_many_with_skips(&mut out, Some(&mut skips), 32, Duration::ZERO);
        assert_eq!(got, 6);
        assert_eq!(skips, vec![(3, 100)]);

        // Also test combining a front drop with a later wire skip.
        let q2 = SampleQueue::new(3);
        let mut b1: Vec<Sample> = vec![sample(0), sample(1), sample(2)];
        q2.push_owned_with_skips(&mut b1, &[(2, 50)]);
        // Push one more sample -> drops sample 0 at front (1 drop skip before sample 1),
        // while keeping the 50-sample wire skip before sample 2.
        q2.push(sample(3));
        let (s1, sk1) = q2.pop_with_skip(Duration::ZERO).unwrap();
        assert_eq!(index(&s1), 1);
        assert_eq!(sk1, 1);
        let (s2, sk2) = q2.pop_with_skip(Duration::ZERO).unwrap();
        assert_eq!(index(&s2), 2);
        assert_eq!(sk2, 50);
    }
}
