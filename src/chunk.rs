//! The buffer that a chunk read fills.

use crate::Scalar;

/// A block of samples that the caller keeps and reuses.
///
/// The values sit end to end, one sample after another. The buffer therefore
/// needs one allocation for its whole life. A program that reads a fast stream
/// makes one of these at startup and passes it to every
/// [`crate::Inlet::pull_chunk`] call.
#[derive(Debug, Clone)]
pub struct Chunk<T> {
    data: Vec<T>,
    stamps: Vec<f64>,
    channels: usize,
    capacity: usize,
}

impl<T: Scalar> Chunk<T> {
    /// A buffer for this many channels and this many samples.
    pub fn new(channels: usize, capacity_samples: usize) -> Chunk<T> {
        let channels = channels.max(1);
        let capacity = capacity_samples.max(1);
        Chunk {
            data: Vec::with_capacity(channels * capacity),
            stamps: Vec::with_capacity(capacity),
            channels,
            capacity,
        }
    }

    /// How many samples the buffer holds now.
    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    /// True when the buffer holds no sample.
    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// How many channels one sample holds.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// How many samples the buffer can hold.
    pub fn capacity_samples(&self) -> usize {
        self.capacity
    }

    /// One sample, as one value for each channel.
    pub fn sample(&self, i: usize) -> &[T] {
        &self.data[i * self.channels..(i + 1) * self.channels]
    }

    /// The timestamp of every sample, in order.
    pub fn timestamps(&self) -> &[f64] {
        &self.stamps
    }

    /// Every sample with its timestamp.
    pub fn iter(&self) -> impl Iterator<Item = (f64, &[T])> + '_ {
        (0..self.len()).map(move |i| (self.stamps[i], self.sample(i)))
    }

    /// Every value of one channel, in order.
    ///
    /// A viewer of signals holds one buffer for each channel, and the samples
    /// arrive one after another. Without this call, each program writes the same
    /// loop. `src/io/lsl.rs` of the g-signals viewer held that loop.
    pub fn channel(&self, c: usize) -> impl Iterator<Item = T> + '_ {
        self.data.iter().skip(c).step_by(self.channels).copied()
    }

    /// Add every value of one channel to the end of `dst`.
    pub fn copy_channel_into(&self, c: usize, dst: &mut Vec<T>) {
        dst.extend(self.channel(c));
    }

    /// Drop every sample. The allocation stays.
    pub fn clear(&mut self) {
        self.data.clear();
        self.stamps.clear();
    }

    /// Add one sample. The inlet calls this.
    #[allow(dead_code)] // the caller is `Inlet::pull_chunk`, which this sketch leaves open
    pub(crate) fn push(&mut self, timestamp: f64, values: impl Iterator<Item = T>) {
        let before = self.data.len();
        self.data.extend(values.take(self.channels));
        self.data.resize(before + self.channels, T::default());
        self.stamps.push(timestamp);
    }
}
