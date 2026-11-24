//! The history of an outlet for the `resume` feature. `docs/extensions.md`,
//! section `resume`.
//!
//! The history holds the last samples of the outlet, with full timestamps,
//! in the order of their `seq` numbers. A consumer that connects again after
//! a dropout gets the samples that it did not receive from here.
//!
//! liblsl has no history. Its outlet makes an empty queue for each new
//! connection (`src/tcp_server.cpp:746`, `src/consumer_queue.cpp:10-20`).

use labstream_wire::{PackedSample, Sample, Value, IRREGULAR_RATE};
use std::collections::VecDeque;

/// The bytes that the history counts for each sample: the timestamp and the
/// number.
const SAMPLE_BYTES: usize = 16;

/// The bytes that the history counts for one value in the legacy [`Sample`]
/// representation, retained for [`sample_bytes`].
const VALUE_BYTES: usize = std::mem::size_of::<Value>();

/// The two limits of a history. The history removes its oldest sample when
/// it reaches one of them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistoryLimits {
    /// The time of samples at the nominal rate. `labstream.ResumeSeconds`.
    /// The value 0 turns the history off.
    pub seconds: f64,
    /// The memory in bytes. `labstream.ResumeMemory`, which is in MiB.
    pub bytes: usize,
}

impl HistoryLimits {
    /// The defaults of `docs/extensions.md`: 30 s and 64 MiB.
    ///
    /// A cut of 10 s, plus the time to connect again, gave a gap of about
    /// 11.8 s in the lossy-link benchmark (`docs/roadmap.md`, step 0). A
    /// default of 10 s lost the oldest samples of that gap.
    pub const DEFAULT: HistoryLimits = HistoryLimits {
        seconds: 30.0,
        bytes: 64 << 20,
    };

    /// A history with these limits holds no sample.
    pub const OFF: HistoryLimits = HistoryLimits {
        seconds: 0.0,
        bytes: 0,
    };

    /// True when these limits let a history hold a sample.
    pub fn allow_history(&self) -> bool {
        self.seconds > 0.0 && self.bytes > 0
    }

    /// The count of samples that the time limit allows at a nominal rate.
    ///
    /// A stream with an irregular rate has no count limit, and uses the
    /// memory limit only.
    fn max_samples(&self, srate: f64) -> usize {
        if srate == IRREGULAR_RATE || !srate.is_finite() || srate < 0.0 {
            return usize::MAX;
        }
        let n = (self.seconds * srate).ceil();
        if n >= usize::MAX as f64 {
            usize::MAX
        } else {
            (n as usize).max(1)
        }
    }
}

impl Default for HistoryLimits {
    fn default() -> Self {
        HistoryLimits::DEFAULT
    }
}

/// The memory that the history counts for one sample.
///
/// The count is 16 bytes, plus the size of a `Value` for each channel, plus
/// the bytes of each string value. `docs/extensions.md`, history rule 5.
pub fn sample_bytes(s: &Sample) -> usize {
    let strings: usize = s
        .values
        .iter()
        .map(|v| match v {
            Value::Str(b) => b.len(),
            _ => 0,
        })
        .sum();
    SAMPLE_BYTES + VALUE_BYTES * s.values.len() + strings
}

/// The memory that the history counts for one [`PackedSample`].
///
/// For numeric streams, this is `16 + channels * format_width` (e.g. 272 bytes
/// for 64 `f32` channels instead of 2064 bytes with `Vec<Value>`).
pub fn packed_sample_bytes(s: &PackedSample) -> usize {
    SAMPLE_BYTES + s.payload_bytes()
}

/// The last samples of an outlet, with their numbers.
///
/// The samples have the numbers `back - len .. back`, with no gap. The
/// owner gives the number of each sample at the push, and the history
/// removes samples only from the front.
#[derive(Debug)]
pub struct History {
    samples: VecDeque<PackedSample>,
    back: u64,
    bytes: usize,
    max_samples: usize,
    max_bytes: usize,
}

impl History {
    /// Build an empty history. The next sample gets the number `next`.
    pub fn new(limits: HistoryLimits, srate: f64, next: u64) -> History {
        History {
            samples: VecDeque::new(),
            back: next,
            bytes: 0,
            max_samples: limits.max_samples(srate),
            max_bytes: limits.bytes,
        }
    }

    /// How many samples the history holds.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when the history holds no sample.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The memory that the history counts now, in bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The number of the oldest sample. With no sample, the number that the
    /// next sample gets.
    pub fn oldest(&self) -> u64 {
        self.back.wrapping_sub(self.samples.len() as u64)
    }

    /// The number that the next sample gets.
    pub fn next(&self) -> u64 {
        self.back
    }

    /// Add one sample. Its timestamp must be a full timestamp, not the
    /// deduced tag.
    ///
    /// The history then removes its oldest samples until it is in its
    /// limits. A sample that is larger than the memory limit is not kept,
    /// but it takes its number.
    pub fn push(&mut self, s: Sample) {
        self.push_packed(PackedSample::from(s));
    }

    /// Add one [`PackedSample`] without copying its `Arc` payload.
    pub fn push_packed(&mut self, s: PackedSample) {
        debug_assert!(s.timestamp != labstream_wire::DEDUCED_TIMESTAMP);
        self.back = self.back.wrapping_add(1);
        let size = packed_sample_bytes(&s);
        if size > self.max_bytes || self.max_samples == 0 {
            self.samples.clear();
            self.bytes = 0;
            return;
        }
        self.bytes += size;
        self.samples.push_back(s);
        while self.samples.len() > self.max_samples || self.bytes > self.max_bytes {
            match self.samples.pop_front() {
                Some(old) => self.bytes -= packed_sample_bytes(&old),
                None => break,
            }
        }
    }

    /// Copy at most `max` samples to the end of `out`, from the number
    /// `from`, and below the number `until`, in order.
    ///
    /// `until` is the first number that a consumer queue holds. The history
    /// also holds the samples from that number, so without the bound a
    /// resend gives a sample twice. `docs/extensions.md`, resend rule 5.
    ///
    /// If `from` is below the oldest number, the copy starts at the oldest
    /// number. The result is the number of the first copied sample, and the
    /// count of copied samples. With no copied sample, the result is `None`.
    pub fn read(
        &self,
        from: u64,
        until: u64,
        max: usize,
        out: &mut Vec<Sample>,
    ) -> Option<(u64, usize)> {
        let mut packed = Vec::new();
        let res = self.read_packed(from, until, max, &mut packed)?;
        out.extend(packed.iter().map(PackedSample::to_sample));
        Some(res)
    }

    /// Share at most `max` packed samples to the end of `out` (`Arc` clone
    /// only, no payload copy), from `from` and below `until`.
    pub fn read_packed(
        &self,
        from: u64,
        until: u64,
        max: usize,
        out: &mut Vec<PackedSample>,
    ) -> Option<(u64, usize)> {
        let oldest = self.oldest();
        // The numbers wrap. A distance from the oldest number gives a
        // position. A distance of more than half the range is a number below
        // the oldest one.
        let len = self.samples.len() as u64;
        let position = |n: u64| -> u64 {
            let d = n.wrapping_sub(oldest);
            if d > u64::MAX / 2 {
                0
            } else {
                d.min(len)
            }
        };
        let start = position(from);
        let end = position(until);
        if start >= end || max == 0 {
            return None;
        }
        let count = ((end - start) as usize).min(max);
        out.extend(
            self.samples
                .range(start as usize..start as usize + count)
                .cloned(),
        );
        Some((oldest.wrapping_add(start), count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(i: u64) -> Sample {
        Sample {
            timestamp: i as f64,
            values: vec![Value::F32(i as f32)],
        }
    }

    fn numbers(out: &[Sample]) -> Vec<u64> {
        out.iter().map(|s| s.timestamp as u64).collect()
    }

    #[test]
    fn the_time_limit_keeps_the_newest_samples() {
        let limits = HistoryLimits {
            seconds: 1.0,
            bytes: usize::MAX,
        };
        let mut h = History::new(limits, 100.0, 0);
        for i in 0..250 {
            h.push(sample(i));
        }
        assert_eq!(h.len(), 100);
        assert_eq!(h.oldest(), 150);
        assert_eq!(h.next(), 250);
    }

    #[test]
    fn the_memory_limit_keeps_the_newest_samples() {
        let one = packed_sample_bytes(&PackedSample::from(sample(0)));
        assert_eq!(one, 16 + 4);
        let limits = HistoryLimits {
            seconds: 1000.0,
            bytes: 10 * one,
        };
        let mut h = History::new(limits, 100.0, 0);
        for i in 0..25 {
            h.push(sample(i));
        }
        assert_eq!(h.len(), 10);
        assert_eq!(h.bytes(), 10 * one);
        assert_eq!(h.oldest(), 15);
    }

    #[test]
    fn an_irregular_stream_uses_the_memory_limit_only() {
        let one = packed_sample_bytes(&PackedSample::from(sample(0)));
        let limits = HistoryLimits {
            seconds: 0.001,
            bytes: 50 * one,
        };
        let mut h = History::new(limits, IRREGULAR_RATE, 0);
        for i in 0..80 {
            h.push(sample(i));
        }
        assert_eq!(h.len(), 50);
    }

    #[test]
    fn a_string_counts_its_bytes() {
        let marker = |text: &str| Sample {
            timestamp: 1.0,
            values: vec![Value::Str(text.as_bytes().to_vec())],
        };
        assert_eq!(sample_bytes(&marker("")), 16 + VALUE_BYTES);
        assert_eq!(sample_bytes(&marker("stimulus")), 16 + VALUE_BYTES + 8);
        let packed_long = |text: &str| packed_sample_bytes(&PackedSample::from(marker(text)));

        // Long markers reach the memory limit first.
        let long = "x".repeat(1000);
        let limits = HistoryLimits {
            seconds: 10.0,
            bytes: 3 * packed_long(&long),
        };
        let mut h = History::new(limits, IRREGULAR_RATE, 0);
        for _ in 0..5 {
            h.push(marker(&long));
        }
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn packed_history_uses_a_fraction_of_value_memory_for_64_channels() {
        let wide = PackedSample::from_f32(1.0, &[0.0f32; 64]);
        // 16 bytes metadata + 64 * 4 bytes payload = 272 bytes (vs 2064 with Vec<Value>).
        assert_eq!(packed_sample_bytes(&wide), 272);
    }

    #[test]
    fn a_sample_larger_than_the_memory_limit_empties_the_history() {
        let limits = HistoryLimits {
            seconds: 10.0,
            bytes: 100,
        };
        let mut h = History::new(limits, IRREGULAR_RATE, 0);
        h.push(sample(0));
        assert_eq!(h.len(), 1);
        h.push(Sample {
            timestamp: 1.0,
            values: vec![Value::Str(vec![b'x'; 200])],
        });
        // The history cannot hold sample 1, and it must not hold sample 0
        // alone, because then the numbers would have a gap.
        assert!(h.is_empty());
        assert_eq!(h.oldest(), 2);
        h.push(sample(2));
        assert_eq!(h.oldest(), 2);
    }

    #[test]
    fn a_read_gives_the_samples_from_the_asked_number() {
        let mut h = History::new(HistoryLimits::DEFAULT, 100.0, 1000);
        for i in 1000..1100 {
            h.push(sample(i));
        }
        let mut out = Vec::new();
        assert_eq!(h.read(1040, 1100, 5, &mut out), Some((1040, 5)));
        assert_eq!(numbers(&out), vec![1040, 1041, 1042, 1043, 1044]);

        // The last block is shorter than the maximum.
        out.clear();
        assert_eq!(h.read(1098, 1100, 256, &mut out), Some((1098, 2)));
        assert_eq!(numbers(&out), vec![1098, 1099]);
    }

    #[test]
    fn a_read_below_the_history_starts_at_the_oldest_sample() {
        let limits = HistoryLimits {
            seconds: 1.0,
            bytes: usize::MAX,
        };
        let mut h = History::new(limits, 10.0, 0);
        for i in 0..30 {
            h.push(sample(i));
        }
        let mut out = Vec::new();
        assert_eq!(h.read(3, 30, 256, &mut out), Some((20, 10)));
        assert_eq!(numbers(&out), (20..30).collect::<Vec<_>>());
    }

    #[test]
    fn a_read_at_or_above_the_next_number_gives_nothing() {
        let mut h = History::new(HistoryLimits::DEFAULT, 100.0, 0);
        let mut out = Vec::new();
        assert_eq!(h.read(0, 10, 256, &mut out), None);
        for i in 0..10 {
            h.push(sample(i));
        }
        assert_eq!(h.read(10, 20, 256, &mut out), None);
        assert_eq!(h.read(5000, 6000, 256, &mut out), None);
        assert_eq!(h.read(5, 10, 0, &mut out), None);
        assert!(out.is_empty());
    }

    #[test]
    fn a_read_stops_below_the_bound() {
        let mut h = History::new(HistoryLimits::DEFAULT, 100.0, 0);
        for i in 0..50 {
            h.push(sample(i));
        }
        let mut out = Vec::new();
        assert_eq!(h.read(10, 20, 256, &mut out), Some((10, 10)));
        assert_eq!(numbers(&out), (10..20).collect::<Vec<_>>());
        // A bound at or below the start gives nothing.
        assert_eq!(h.read(20, 20, 256, &mut Vec::new()), None);
        assert_eq!(h.read(30, 20, 256, &mut Vec::new()), None);
        // A bound below the oldest number gives nothing.
        let limits = HistoryLimits {
            seconds: 0.1,
            bytes: usize::MAX,
        };
        let mut small = History::new(limits, 100.0, 0);
        for i in 0..50 {
            small.push(sample(i));
        }
        assert_eq!(small.oldest(), 40);
        assert_eq!(small.read(0, 30, 256, &mut Vec::new()), None);
    }

    #[test]
    fn the_numbers_can_wrap() {
        let start = u64::MAX - 4;
        let mut h = History::new(HistoryLimits::DEFAULT, 100.0, start);
        for i in 0..10u64 {
            h.push(sample(i));
        }
        assert_eq!(h.oldest(), start);
        assert_eq!(h.next(), 5);
        let mut out = Vec::new();
        assert_eq!(h.read(1, 5, 256, &mut out), Some((1, 4)));
        assert_eq!(numbers(&out), vec![6, 7, 8, 9]);
        out.clear();
        assert_eq!(h.read(start - 3, 5, 2, &mut out), Some((start, 2)));
        assert_eq!(numbers(&out), vec![0, 1]);
    }

    #[test]
    fn the_default_history_holds_30_s() {
        assert_eq!(HistoryLimits::DEFAULT.seconds, 30.0);
        assert_eq!(HistoryLimits::DEFAULT.bytes, 64 << 20);
        // At 512 Hz x 8 channels, the time limit acts first.
        let mut h = History::new(HistoryLimits::DEFAULT, 512.0, 0);
        let s = Sample {
            timestamp: 1.0,
            values: vec![Value::F32(0.0); 8],
        };
        for _ in 0..40 * 512 {
            h.push(s.clone());
        }
        assert_eq!(h.len(), 30 * 512);
    }

    #[test]
    fn the_limits_can_turn_the_history_off() {
        assert!(HistoryLimits::DEFAULT.allow_history());
        assert!(!HistoryLimits::OFF.allow_history());
        let no_time = HistoryLimits {
            seconds: 0.0,
            bytes: 1 << 20,
        };
        assert!(!no_time.allow_history());
    }
}
