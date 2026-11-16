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

use labstream_wire::Sample;
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
    q: VecDeque<Sample>,
    back: u64,
}

impl std::ops::Deref for Ring {
    type Target = VecDeque<Sample>;
    fn deref(&self) -> &VecDeque<Sample> {
        &self.q
    }
}

impl std::ops::DerefMut for Ring {
    fn deref_mut(&mut self) -> &mut VecDeque<Sample> {
        &mut self.q
    }
}

impl Ring {
    /// The number of the sample at the front.
    fn front_number(&self) -> u64 {
        self.back.wrapping_sub(self.q.len() as u64)
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
        let capacity = capacity.max(1);
        SampleQueue {
            inner: Mutex::new(Ring {
                q: VecDeque::with_capacity(capacity.min(4096)),
                back: first,
            }),
            room: Condvar::new(),
            capacity,
            dropped: AtomicU64::new(0),
            closed: Mutex::new(false),
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
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            q.pop_front();
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
        let mut q = self.inner.lock().unwrap();
        let mut lost = 0u64;
        for s in samples {
            if q.len() >= self.capacity {
                q.pop_front();
                lost += 1;
            }
            q.push_back(s.clone());
        }
        q.back = q.back.wrapping_add(samples.len() as u64);
        drop(q);
        if lost > 0 {
            self.dropped.fetch_add(lost, Ordering::SeqCst);
        }
        // A block can satisfy more than one reader, so every reader wakes.
        self.room.notify_all();
    }

    /// Move every sample out of `samples` into the queue, with one lock.
    ///
    /// The result is the same as [`SampleQueue::push_many`], but no sample is
    /// copied. `samples` is empty after the call and keeps its capacity, so
    /// the caller can fill it again with no new allocation.
    pub fn push_owned(&self, samples: &mut Vec<Sample>) {
        if samples.is_empty() {
            return;
        }
        let mut q = self.inner.lock().unwrap();
        let mut lost = 0u64;
        // Samples that the queue drops at once are never moved in.
        let skip = samples.len().saturating_sub(self.capacity);
        // Each sample takes a number, also the ones that are never moved in.
        q.back = q.back.wrapping_add(samples.len() as u64);
        for s in samples.drain(..).skip(skip) {
            if q.len() >= self.capacity {
                q.pop_front();
                lost += 1;
            }
            q.push_back(s);
        }
        drop(q);
        lost += skip as u64;
        if lost > 0 {
            self.dropped.fetch_add(lost, Ordering::SeqCst);
        }
        self.room.notify_all();
    }

    /// Take the oldest sample, waiting up to `timeout`.
    ///
    /// Returns `None` when the timeout passes with nothing waiting, or when the
    /// queue is closed and empty.
    pub fn pop(&self, timeout: Duration) -> Option<Sample> {
        let end = Instant::now() + timeout;
        let mut q = self.inner.lock().unwrap();
        loop {
            if let Some(s) = q.pop_front() {
                return Some(s);
            }
            if *self.closed.lock().unwrap() {
                return None;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let (guard, result) = self
                .room
                .wait_timeout(q, left.min(Duration::from_millis(100)))
                .unwrap();
            q = guard;
            if result.timed_out() && Instant::now() >= end {
                return q.pop_front();
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
        if max == 0 {
            return 0;
        }
        let end = Instant::now() + timeout;
        let mut q = self.inner.lock().unwrap();
        loop {
            if !q.is_empty() {
                let take = q.len().min(max);
                out.extend(q.drain(..take));
                return take;
            }
            if *self.closed.lock().unwrap() {
                return 0;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return 0;
            }
            let (guard, result) = self
                .room
                .wait_timeout(q, left.min(Duration::from_millis(100)))
                .unwrap();
            q = guard;
            if result.timed_out() && Instant::now() >= end {
                let take = q.len().min(max);
                out.extend(q.drain(..take));
                return take;
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
        if max == 0 {
            return (0, self.inner.lock().unwrap().front_number());
        }
        let end = Instant::now() + timeout;
        let mut q = self.inner.lock().unwrap();
        loop {
            if !q.is_empty() || Instant::now() >= end || *self.closed.lock().unwrap() {
                let first = q.front_number();
                let take = q.len().min(max);
                out.extend(q.drain(..take));
                return (take, first);
            }
            let left = end.saturating_duration_since(Instant::now());
            let (guard, _) = self
                .room
                .wait_timeout(q, left.min(Duration::from_millis(100)))
                .unwrap();
            q = guard;
        }
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
}

/// A set of consumers that all receive the same sample.
///
/// liblsl calls this the send buffer (`src/send_buffer.h`). Each consumer holds
/// its own queue, so a slow consumer loses only its own samples. SPEC.md 8.8.
#[derive(Debug, Default)]
pub struct Fanout {
    consumers: Mutex<Consumers>,
}

/// The queues of a fanout, and the number of the next pushed sample.
///
/// One lock holds both, so a new queue starts at the number that the next
/// push gives, and every queue sees the same numbers.
#[derive(Debug, Default)]
struct Consumers {
    list: Vec<Arc<SampleQueue>>,
    next: u64,
}

impl Fanout {
    /// Build an empty set.
    pub fn new() -> Self {
        Fanout::default()
    }

    /// Add a consumer with its own queue.
    pub fn add(&self, capacity: usize) -> Arc<SampleQueue> {
        let mut set = self.consumers.lock().unwrap();
        let q = Arc::new(SampleQueue::with_first_number(capacity, set.next));
        set.list.push(Arc::clone(&q));
        q
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
            .retain(|c| !Arc::ptr_eq(c, q));
    }

    /// How many consumers are connected.
    pub fn count(&self) -> usize {
        self.consumers.lock().unwrap().list.len()
    }

    /// Give one sample to every consumer.
    ///
    /// A consumer that has gone away is dropped from the set. The call never
    /// waits, whatever any consumer is doing.
    pub fn push(&self, s: &Sample) {
        let mut set = self.consumers.lock().unwrap();
        set.next = set.next.wrapping_add(1);
        set.list.retain(|q| !q.is_closed());
        for q in set.list.iter() {
            q.push(s.clone());
        }
    }

    /// Give several samples to every consumer, with one lock for the set.
    ///
    /// The result for each consumer is the same as one [`Fanout::push`] for each
    /// sample, in order.
    pub fn push_many(&self, samples: &[Sample]) {
        if samples.is_empty() {
            return;
        }
        let mut set = self.consumers.lock().unwrap();
        set.next = set.next.wrapping_add(samples.len() as u64);
        set.list.retain(|q| !q.is_closed());
        for q in set.list.iter() {
            q.push_many(samples);
        }
    }

    /// Close every consumer queue.
    pub fn close(&self) {
        for q in self.consumers.lock().unwrap().list.iter() {
            q.close();
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
}
